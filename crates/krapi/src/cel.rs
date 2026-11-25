//! THE CEL RUNTIME — every `${...}` expression evaluates through here.
//! The expression language rides the `cel` crate's stdlib environment
//! (optional syntax `a.?b` / `orValue`, the standard macros) extended
//! with the cel-go extension libraries upstream kro registers (strings,
//! lists, encoders, network, sets, bindings, two-variable
//! comprehensions) and kro's own function library in its upstream
//! shapes. The Kubernetes-only libraries upstream loads from
//! k8s.io/apiserver — `find`/`findAll`, the URL family and
//! `quantity` — are hand-ported beside them.
//!
//! Values cross the JSON boundary in both directions (the cluster
//! speaks JSON; CEL speaks [`Value`]). `omit()` is the one sentinel
//! value in the language: an expression that evaluates to it makes its
//! template field disappear entirely.

use crate::expression::Render;
use cel::common::ast::{CallExpr, EntryExpr, Expr, IdedExpr, LiteralValue, MapEntryExpr, MapExpr};
use cel::extractors::{Arguments, This};
use cel::objects::{Key, Map, Opaque};
use cel::parser::Macro;
use cel::{Context, Env, ExecutionError, ResolveResult, Value};
use md5::Digest as _;
use serde_json::Value as Json;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

/// The `omit()` sentinel — a template field whose expression renders to
/// this marker is removed from the rendered object. The type name is
/// upstream kro's (`kro.omit`).
#[derive(Debug, PartialEq, Eq)]
pub struct OmitMarker;

impl Opaque for OmitMarker {
    fn runtime_type_name(&self) -> &'static str {
        "kro.omit"
    }
}

/// Is this CEL value the omit sentinel?
#[must_use]
pub fn is_omit(v: &Value) -> bool {
    matches!(v, Value::Opaque(o) if o.runtime_type_name() == "kro.omit")
}

/// The quantity opaque rides the CEL env as a first-class value.
impl Opaque for crate::quantity::Quantity {
    fn runtime_type_name(&self) -> &'static str {
        "kubernetes.Quantity"
    }
}

/// JSON → CEL.
#[must_use]
pub fn json_to_cel(v: &Json) -> Value {
    match v {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(u) = n.as_u64() {
                Value::UInt(u)
            } else {
                Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        Json::String(s) => Value::String(Arc::new(s.clone())),
        Json::Array(items) => Value::List(Arc::new(items.iter().map(json_to_cel).collect())),
        Json::Object(map) => Value::Map(Map {
            map: Arc::new(
                map.iter()
                    .map(|(k, v)| (Key::String(Arc::new(k.clone())), json_to_cel(v)))
                    .collect(),
            ),
        }),
    }
}

/// CEL → JSON. The omit sentinel is not a JSON value — callers check
/// [`is_omit`] before calling.
///
/// # Errors
/// The value is a function or an opaque the cluster cannot represent.
pub fn cel_to_json(v: &Value) -> Result<Json, String> {
    v.json().map_err(|e| format!("{e}"))
}

/// The evaluation scope: CEL variable name → JSON value.
pub type Scope = BTreeMap<String, Json>;

/// The shared environment: the CEL stdlib (macros, optional syntax) plus
/// the extension libraries upstream kro loads, plus the
/// `runtime.newCondition` map-literal macro. Built once; programs
/// compiled through it expand the macros and accept the optional
/// syntax. `Program::compile` would parse bare (no macros, no
/// optional), so every compile goes through this env.
fn env() -> Arc<Env> {
    static ENV: OnceLock<Arc<Env>> = OnceLock::new();
    ENV.get_or_init(|| {
        let mut env = Env::stdlib();
        for register in [
            cel::extensions::strings,
            cel::extensions::lists,
            cel::extensions::encoders,
            cel::extensions::network,
            cel::extensions::sets,
            cel::extensions::bindings,
            cel::extensions::two_var_comprehensions,
        ] {
            if let Err(e) = register(&mut env) {
                // Unreachable on a fresh env: the extension set is fixed
                // and carries no conflicts with the stdlib.
                tracing::error!("kro cel: extension registration failed: {e}");
            }
        }
        if let Err(e) = env.add_macro(new_condition_macro()) {
            tracing::error!("kro cel: newCondition macro registration failed: {e}");
        }
        Arc::new(env)
    })
    .clone()
}

/// Compile one expression against the shared environment.
fn program(expr: &str) -> Result<cel::Program, String> {
    env().compile(expr).map_err(|e| e.to_string())
}

/// The `runtime.newCondition({type, status, reason, message})` macro —
/// upstream kro's parse-time pass over the map literal: bare identifier
/// keys only (quoted or computed keys refused), known keys only, no
/// duplicates, `type` and `status` required. The keys rewrite into
/// string literals; evaluation lands in [`new_condition`] with the same
/// validations for computed values.
fn new_condition_macro() -> Macro {
    Macro::receiver("newCondition", 1, |helper, target, args| {
        // Decline first (the helper contract): only `runtime` carries it.
        if !matches!(target.as_ref().map(|t| &t.expr), Some(Expr::Ident(n)) if n == "runtime") {
            return Ok(None);
        }
        let runtime = target.take().unwrap_or_default();
        let Some(arg) = args.pop() else {
            return Ok(None);
        };
        let Expr::Map(m) = arg.expr else {
            return Err(helper.new_error(
                arg.id,
                "runtime.newCondition: argument must be a map literal",
            ));
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut entries = Vec::with_capacity(m.entries.len());
        for entry in m.entries {
            let EntryExpr::MapEntry(me) = entry.expr else {
                return Err(helper.new_error(entry.id, "runtime.newCondition: bad map entry"));
            };
            let Expr::Ident(name) = me.key.expr else {
                return Err(helper.new_error(
                    me.key.id,
                    "runtime.newCondition: keys must be bare identifiers (type, status, reason, message); quoted or computed keys are not allowed",
                ));
            };
            if !matches!(name.as_str(), "type" | "status" | "reason" | "message") {
                return Err(helper.new_error(
                    me.key.id,
                    format!(
                        "runtime.newCondition: unknown key {name:?} (allowed: type, status, reason, message)"
                    ),
                ));
            }
            if !seen.insert(name.clone()) {
                return Err(helper.new_error(
                    me.key.id,
                    format!("runtime.newCondition: duplicate key {name:?}"),
                ));
            }
            let key = helper.next_expr(Expr::Literal(LiteralValue::String(
                cel::common::types::CelString::from(name.clone()),
            )));
            let value = IdedExpr {
                id: me.value.id,
                expr: me.value.expr,
            };
            entries.push(cel::common::ast::IdedEntryExpr {
                id: entry.id,
                expr: EntryExpr::MapEntry(MapEntryExpr {
                    key,
                    value,
                    optional: false,
                }),
            });
        }
        for required in ["type", "status"] {
            if !seen.contains(required) {
                return Err(helper.new_error(
                    arg.id,
                    format!("runtime.newCondition: {required:?} is required"),
                ));
            }
        }
        let map = helper.next_expr(Expr::Map(MapExpr { entries }));
        Ok(Some(helper.next_expr(Expr::Call(CallExpr {
            func_name: "newCondition".to_owned(),
            target: Some(Box::new(runtime)),
            args: vec![map],
        }))))
    })
}

/// Register the kro function library onto one evaluation context. The
/// `This` extractor receives the member-call target — and falls back to
/// the first argument for global calls, so every function here answers
/// both `x.f(y)` and `f(x, y)` where upstream declares a member.
#[allow(clippy::too_many_lines)] // the function library reads best as one flat register
fn register_functions(ctx: &mut Context<'static, 'static>) {
    // merge(dst, src): shallow — src's keys win, dst fills the gaps.
    reg(
        ctx,
        "merge",
        |This(dst): This<Value>, src: Value| -> ResolveResult {
            Ok(Value::Map(merge_maps(&dst, &src, false)?))
        },
    );
    // deepMerge(dst, src): recursive — nested maps merge, src's leaves win.
    reg(
        ctx,
        "deepMerge",
        |This(dst): This<Value>, src: Value| -> ResolveResult {
            Ok(Value::Map(merge_maps(&dst, &src, true)?))
        },
    );
    // omit(): the field-omission sentinel (upstream: zero-arg only — a
    // field renders it away; there is no key-filtering form).
    reg(ctx, "omit", |args: Arguments| -> ResolveResult {
        if args.0.is_empty() {
            Ok(Value::Opaque(Arc::new(OmitMarker)))
        } else {
            Err(cel::ExecutionError::function_error(
                "omit",
                "omit() takes no arguments: the field carrying its value is removed from the render",
            ))
        }
    });
    // hash.fnv64a(s) / hash.sha256(s) / hash.md5(s): raw digest bytes
    // (upstream; hex/base64 is the author's `"%x".format` /
    // `base64.encode` choice).
    reg(ctx, "hash.fnv64a", |s: Arc<String>| -> ResolveResult {
        Ok(Value::Bytes(Arc::new(
            fnv64a(s.as_bytes()).to_be_bytes().to_vec(),
        )))
    });
    reg(ctx, "hash.sha256", |s: Arc<String>| -> ResolveResult {
        Ok(Value::Bytes(Arc::new(
            sha2::Sha256::digest(s.as_bytes()).to_vec(),
        )))
    });
    reg(ctx, "hash.md5", |s: Arc<String>| -> ResolveResult {
        Ok(Value::Bytes(Arc::new(
            md5::Md5::digest(s.as_bytes()).to_vec(),
        )))
    });
    // json.marshal(v): compact JSON text. json.unmarshal(s): the value.
    reg(ctx, "json.marshal", |v: Value| -> ResolveResult {
        let j =
            cel_to_json(&v).map_err(|e| cel::ExecutionError::function_error("json.marshal", e))?;
        serde_json::to_string(&j)
            .map(|s| Value::String(Arc::new(s)))
            .map_err(|e| cel::ExecutionError::function_error("json.marshal", e.to_string()))
    });
    reg(ctx, "json.unmarshal", |s: Arc<String>| -> ResolveResult {
        serde_json::from_str::<Json>(&s)
            .map(|j| json_to_cel(&j))
            .map_err(|e| cel::ExecutionError::function_error("json.unmarshal", format!("{e}")))
    });
    // random.seededString(length, seed): a deterministic alphanumeric
    // string (upstream algorithm: sha256 windows over
    // "0123456789abcdefghijklmnopqrstuvwxyz", rehashing on exhaustion).
    reg(
        ctx,
        "random.seededString",
        |len: i64, seed: Arc<String>| -> ResolveResult { seeded_string(len, seed.as_bytes()) },
    );
    // random.seededInt(min, max, seed): a deterministic integer in [min, max).
    reg(
        ctx,
        "random.seededInt",
        |min: i64, max: i64, seed: Arc<String>| -> ResolveResult {
            seeded_int(min, max, seed.as_bytes())
        },
    );
    // lists.setAtIndex / insertAtIndex / removeAtIndex: pure index
    // mutations (upstream bounds: set/remove in [0, size), insert in
    // [0, size]).
    reg(
        ctx,
        "lists.setAtIndex",
        |args: Arguments| -> ResolveResult {
            let (list, idx, value) = three_args(&args)?;
            let mut items = list_items(&list, "lists.setAtIndex")?;
            let i = list_index(&idx, items.len(), "lists.setAtIndex")?;
            items[i] = value;
            Ok(Value::List(Arc::new(items)))
        },
    );
    reg(
        ctx,
        "lists.insertAtIndex",
        |args: Arguments| -> ResolveResult {
            let (list, idx, value) = three_args(&args)?;
            let mut items = list_items(&list, "lists.insertAtIndex")?;
            let i = list_index(&idx, items.len() + 1, "lists.insertAtIndex")?;
            items.insert(i, value);
            Ok(Value::List(Arc::new(items)))
        },
    );
    reg(
        ctx,
        "lists.removeAtIndex",
        |args: Arguments| -> ResolveResult {
            let (list, idx) = two_args(&args)?;
            let mut items = list_items(&list, "lists.removeAtIndex")?;
            let i = list_index(&idx, items.len(), "lists.removeAtIndex")?;
            items.remove(i);
            Ok(Value::List(Arc::new(items)))
        },
    );
    // runtime.newCondition: the map-literal macro lands here after its
    // parse-time key validation; computed maps ride the same function.
    reg(ctx, "runtime.newCondition", new_condition_fn);
    // The macro's rewritten member call resolves `runtime.newCondition`
    // as a qualified global before it ever touches the target, so the
    // `runtime` receiver itself is never resolved. `condition(obj, type)`
    // has no macro; the qualified form carries it directly.
    reg(
        ctx,
        "runtime.condition",
        |obj: Value, wanted: Arc<String>| -> ResolveResult { condition_of(&obj, &wanted) },
    );
    // — the Kubernetes CEL libraries upstream loads from
    //   k8s.io/apiserver and cel-rust does not ship —
    // regex: `<string>.find(re)` first match or "";
    // `<string>.findAll(re[, n])` all / first-n matches.
    reg(
        ctx,
        "find",
        |This(s): This<Value>, re: Arc<String>| -> ResolveResult {
            let s = cel_to_json(&s)
                .ok()
                .and_then(|j| j.as_str().map(str::to_owned))
                .ok_or_else(|| {
                    cel::ExecutionError::function_error("find", "the receiver must be a string")
                })?;
            let hits = regex_hits(&s, &re, None, "find")?;
            Ok(Value::String(Arc::new(
                hits.into_iter().next().unwrap_or_default(),
            )))
        },
    );
    reg(
        ctx,
        "findAll",
        |This(s): This<Value>, args: Arguments| -> ResolveResult {
            let s = cel_to_json(&s)
                .ok()
                .and_then(|j| j.as_str().map(str::to_owned))
                .ok_or_else(|| {
                    cel::ExecutionError::function_error("findAll", "the receiver must be a string")
                })?;
            let re = string_at(&args, 0, "findAll")?;
            let limit = args.0.get(1).and_then(|v| match v {
                Value::Int(n) => Some(*n),
                Value::UInt(n) => i64::try_from(*n).ok(),
                _ => None,
            });
            let hits = regex_hits(&s, &re, limit, "findAll")?;
            Ok(Value::List(Arc::new(
                hits.into_iter()
                    .map(|h| Value::String(Arc::new(h)))
                    .collect(),
            )))
        },
    );
    // url family (k8s URL library): url(s) → URL, isURL(s) → bool, and
    // the getters on the parsed URL.
    reg(ctx, "url", |s: Arc<String>| -> ResolveResult {
        parsed_url(&s)
            .map(|u| Value::Opaque(Arc::new(u)))
            .map_err(|e| cel::ExecutionError::function_error("url", e))
    });
    reg(ctx, "isURL", |s: Arc<String>| -> ResolveResult {
        Ok(Value::Bool(parsed_url(&s).is_ok()))
    });
    for (name, pick) in [
        ("getScheme", UrlGet::Scheme),
        ("getHost", UrlGet::Host),
        ("getHostname", UrlGet::Hostname),
        ("getPort", UrlGet::Port),
        ("getEscapedPath", UrlGet::Path),
    ] {
        reg(ctx, name, move |This(u): This<Value>| -> ResolveResult {
            let Value::Opaque(o) = &u else {
                return Err(cel::ExecutionError::function_error(
                    name,
                    format!("{name} needs a url() value"),
                ));
            };
            let Some(url) = o.downcast_ref::<UrlValue>() else {
                return Err(cel::ExecutionError::function_error(
                    name,
                    format!("{name} needs a url() value"),
                ));
            };
            Ok(Value::String(Arc::new(url.pick(pick))))
        });
    }
    reg(ctx, "getQuery", |This(u): This<Value>| -> ResolveResult {
        let Value::Opaque(o) = &u else {
            return Err(cel::ExecutionError::function_error(
                "getQuery",
                "getQuery needs a url() value",
            ));
        };
        let Some(url) = o.downcast_ref::<UrlValue>() else {
            return Err(cel::ExecutionError::function_error(
                "getQuery",
                "getQuery needs a url() value",
            ));
        };
        let mut map = HashMap::new();
        for (k, vs) in url.url.query_pairs() {
            map.entry(Key::String(Arc::new(k.into_owned())))
                .or_insert_with(Vec::new)
                .push(Value::String(Arc::new(vs.into_owned())));
        }
        Ok(Value::Map(Map {
            map: Arc::new(
                map.into_iter()
                    .map(|(k, vs)| (k, Value::List(Arc::new(vs))))
                    .collect(),
            ),
        }))
    });
    // quantity family (k8s quantity library): quantity(s) → Quantity,
    // isQuantity(s) → bool, and the arithmetic/comparison members.
    reg(ctx, "quantity", |s: Arc<String>| -> ResolveResult {
        crate::quantity::Quantity::parse(&s)
            .map(|q| Value::Opaque(Arc::new(q)))
            .map_err(|e| cel::ExecutionError::function_error("quantity", e))
    });
    reg(ctx, "isQuantity", |s: Arc<String>| -> ResolveResult {
        Ok(Value::Bool(crate::quantity::Quantity::parse(&s).is_ok()))
    });
    reg(
        ctx,
        "add",
        |This(a): This<Value>, b: Value| -> ResolveResult {
            match qty_op(&a, &b, "add", i128::checked_add) {
                Ok(q) => Ok(Value::Opaque(Arc::new(q))),
                Err(e) => Err(e),
            }
        },
    );
    reg(
        ctx,
        "sub",
        |This(a): This<Value>, b: Value| -> ResolveResult {
            match qty_op(&a, &b, "sub", i128::checked_sub) {
                Ok(q) => Ok(Value::Opaque(Arc::new(q))),
                Err(e) => Err(e),
            }
        },
    );
    reg(
        ctx,
        "isLessThan",
        |This(a): This<Value>, b: Value| -> ResolveResult {
            Ok(Value::Bool(
                qty_cmp(&a, &b, "isLessThan")? == std::cmp::Ordering::Less,
            ))
        },
    );
    reg(
        ctx,
        "isGreaterThan",
        |This(a): This<Value>, b: Value| -> ResolveResult {
            Ok(Value::Bool(
                qty_cmp(&a, &b, "isGreaterThan")? == std::cmp::Ordering::Greater,
            ))
        },
    );
    reg(
        ctx,
        "compareTo",
        |This(a): This<Value>, b: Value| -> ResolveResult {
            Ok(Value::Int(match qty_cmp(&a, &b, "compareTo")? {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        },
    );
    reg(ctx, "sign", |This(a): This<Value>| -> ResolveResult {
        let q = qty_of(&a, "sign")?;
        Ok(Value::Int(i64::from(q.sign())))
    });
    reg(ctx, "isInteger", |This(a): This<Value>| -> ResolveResult {
        let q = qty_of(&a, "isInteger")?;
        Ok(Value::Bool(q.as_integer().is_some()))
    });
    reg(ctx, "asInteger", |This(a): This<Value>| -> ResolveResult {
        let q = qty_of(&a, "asInteger")?;
        q.as_integer().map(Value::Int).ok_or_else(|| {
            cel::ExecutionError::function_error(
                "asInteger",
                "the quantity cannot be represented as an integer without loss",
            )
        })
    });
    reg(
        ctx,
        "asApproximateFloat",
        |This(a): This<Value>| -> ResolveResult {
            let q = qty_of(&a, "asApproximateFloat")?;
            Ok(Value::Float(q.as_float()))
        },
    );
}

/// Register one function; a refusal means the name collided with the
/// stdlib or an extension — a build-ordering bug, surfaced in the log.
fn reg<T: 'static, F>(ctx: &mut Context<'static, 'static>, name: &'static str, f: F)
where
    F: cel::extractors::IntoFunction<T> + 'static + Send + Sync,
{
    if ctx.add_function(name, f).is_err() {
        tracing::error!("kro cel: function {name:?} collided with a stdlib overload");
    }
}

fn missing_arg() -> ExecutionError {
    ExecutionError::function_error("kro", "missing argument")
}

fn two_args(args: &Arguments) -> Result<(Value, Value), ExecutionError> {
    let a = args.0.first().cloned().ok_or_else(missing_arg)?;
    let b = args.0.get(1).cloned().ok_or_else(missing_arg)?;
    Ok((a, b))
}

fn three_args(args: &Arguments) -> Result<(Value, Value, Value), ExecutionError> {
    let a = args.0.first().cloned().ok_or_else(missing_arg)?;
    let b = args.0.get(1).cloned().ok_or_else(missing_arg)?;
    let c = args.0.get(2).cloned().ok_or_else(missing_arg)?;
    Ok((a, b, c))
}

fn string_at(args: &Arguments, i: usize, f: &str) -> Result<Arc<String>, ExecutionError> {
    match args.0.get(i) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(cel::ExecutionError::function_error(
            f,
            format!("argument {i} must be a string"),
        )),
    }
}

fn list_items(v: &Value, f: &str) -> Result<Vec<Value>, ExecutionError> {
    match v {
        Value::List(items) => Ok(items.as_ref().clone()),
        _ => Err(cel::ExecutionError::function_error(
            f,
            "the first argument must be a list",
        )),
    }
}

fn list_index(v: &Value, len: usize, f: &str) -> Result<usize, ExecutionError> {
    let i = match v {
        Value::Int(n) => *n,
        Value::UInt(n) => i64::try_from(*n)
            .map_err(|_| cel::ExecutionError::function_error(f, "index out of range"))?,
        _ => {
            return Err(cel::ExecutionError::function_error(
                f,
                "the index must be an integer",
            ));
        }
    };
    let u = usize::try_from(i).map_err(|_| {
        cel::ExecutionError::function_error(f, format!("index {i} out of bounds [0, {len})"))
    })?;
    if u >= len {
        return Err(cel::ExecutionError::function_error(
            f,
            format!("index {i} out of bounds [0, {len})"),
        ));
    }
    Ok(u)
}

/// FNV-1a 64.
fn fnv64a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Upstream `random.seededString`: sha256 over the seed, 4-byte windows
/// mapping into `0123456789abcdefghijklmnopqrstuvwxyz`; when the window
/// runs past the digest, rehash `sha256(digest || generated)` and reset.
fn seeded_string(len: i64, seed: &[u8]) -> ResolveResult {
    if len <= 0 {
        return Err(cel::ExecutionError::function_error(
            "random.seededString",
            "length must be positive",
        ));
    }
    let alphabet = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let n = usize::try_from(len).unwrap_or(0);
    let mut hash = sha2::Sha256::digest(seed).to_vec();
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let mut start = (i * 4) % hash.len();
        if start + 4 > hash.len() {
            hash = sha2::Sha256::digest(
                hash.iter()
                    .chain(out.iter())
                    .copied()
                    .collect::<Vec<u8>>()
                    .as_slice(),
            )
            .to_vec();
            start = 0;
        }
        let idx = u32::from(hash[start]) << 24
            | u32::from(hash[start + 1]) << 16
            | u32::from(hash[start + 2]) << 8
            | u32::from(hash[start + 3]);
        out.push(alphabet[idx as usize % alphabet.len()]);
        i += 1;
    }
    Ok(Value::String(Arc::new(
        String::from_utf8(out).unwrap_or_default(),
    )))
}

/// Upstream `random.seededInt`: the digest's first 8 bytes as a
/// big-endian u64 folded into `[min, max)`.
fn seeded_int(min: i64, max: i64, seed: &[u8]) -> ResolveResult {
    if min >= max {
        return Err(cel::ExecutionError::function_error(
            "random.seededInt",
            "min must be less than max",
        ));
    }
    let d = sha2::Sha256::digest(seed);
    let mut v = 0u64;
    for b in d.iter().take(8) {
        v = (v << 8) | u64::from(*b);
    }
    let span = u64::try_from(max - min)
        .map_err(|_| cel::ExecutionError::function_error("random.seededInt", "range overflow"))?;
    let step = i64::try_from(v % span)
        .map_err(|_| cel::ExecutionError::function_error("random.seededInt", "range overflow"))?;
    let result = min
        .checked_add(step)
        .ok_or_else(|| cel::ExecutionError::function_error("random.seededInt", "range overflow"))?;
    Ok(Value::Int(result))
}

/// Shallow or deep map merge; src wins. Both operands must be maps
/// (upstream refuses non-maps instead of treating them as empty).
fn merge_maps(dst: &Value, src: &Value, deep: bool) -> Result<Map, ExecutionError> {
    let (Value::Map(d), Value::Map(s)) = (dst, src) else {
        return Err(cel::ExecutionError::function_error(
            "merge",
            "merge expects two maps",
        ));
    };
    let mut map = d.map.as_ref().clone();
    for (k, v) in s.map.iter() {
        let merged = if deep
            && let Some(existing) = map.get(k).cloned()
            && matches!(existing, Value::Map(_))
            && matches!(v, Value::Map(_))
        {
            {
                let m = merge_maps(&existing, v, true)?;
                Value::Map(m)
            }
        } else {
            v.clone()
        };
        map.insert(k.clone(), merged);
    }
    Ok(Map { map: Arc::new(map) })
}

/// `runtime.newCondition(map)` — the string-checked constructor. The
/// macro has already rewritten and validated the keys; this validates
/// the values (strings, non-empty type, status in True/False/Unknown).
fn new_condition_fn(Arguments(args): Arguments) -> ResolveResult {
    let Value::Map(m) = args.first().cloned().ok_or_else(missing_arg)? else {
        return Err(cel::ExecutionError::function_error(
            "newCondition",
            "argument must be a map",
        ));
    };
    let read = |key: &str| -> Result<String, ExecutionError> {
        match m.map.get(&Key::String(Arc::new(key.to_owned()))) {
            None | Some(Value::Null) => Ok(String::new()),
            Some(Value::String(s)) => Ok(s.to_string()),
            Some(_) => Err(cel::ExecutionError::function_error(
                "newCondition",
                format!("{key} must be a string"),
            )),
        }
    };
    let ctype = read("type")?;
    let status = read("status")?;
    if ctype.is_empty() {
        return Err(cel::ExecutionError::function_error(
            "newCondition",
            "type must not be empty",
        ));
    }
    if !matches!(status.as_str(), "True" | "False" | "Unknown") {
        return Err(cel::ExecutionError::function_error(
            "newCondition",
            format!("status must be one of True, False, Unknown (got {status:?})"),
        ));
    }
    Ok(Value::Map(Map {
        map: Arc::new(
            [
                ("type", ctype),
                ("status", status),
                ("reason", read("reason")?),
                ("message", read("message")?),
            ]
            .into_iter()
            .map(|(k, v)| {
                (
                    Key::String(Arc::new(k.to_owned())),
                    Value::String(Arc::new(v)),
                )
            })
            .collect(),
        ),
    }))
}

/// `runtime.condition(obj, type)` — look a condition up on
/// `obj.status.conditions[]`; a missing path yields the empty condition
/// (upstream's zero Condition), a wrong shape is an error.
fn condition_of(obj: &Value, wanted: &str) -> ResolveResult {
    let Value::Map(m) = obj else {
        return Err(cel::ExecutionError::function_error(
            "runtime.condition",
            "obj must be an object",
        ));
    };
    let Some(status) = m.map.get(&Key::String(Arc::new("status".to_owned()))) else {
        return empty_condition();
    };
    let Value::Map(status) = status else {
        return Err(cel::ExecutionError::function_error(
            "runtime.condition",
            "obj.status must be an object",
        ));
    };
    let Some(conds) = status
        .map
        .get(&Key::String(Arc::new("conditions".to_owned())))
    else {
        return empty_condition();
    };
    let Value::List(conds) = conds else {
        return Err(cel::ExecutionError::function_error(
            "runtime.condition",
            "obj.status.conditions must be a list",
        ));
    };
    for cond in conds.iter() {
        let Value::Map(c) = cond else {
            return Err(cel::ExecutionError::function_error(
                "runtime.condition",
                "conditions must be objects",
            ));
        };
        let field = |k: &str| -> String {
            match c.map.get(&Key::String(Arc::new(k.to_owned()))) {
                Some(Value::String(s)) => s.to_string(),
                _ => String::new(),
            }
        };
        if field("type") == wanted {
            return Ok(Value::Map(Map {
                map: Arc::new(
                    [
                        ("type", field("type")),
                        ("status", field("status")),
                        ("reason", field("reason")),
                        ("message", field("message")),
                    ]
                    .into_iter()
                    .map(|(k, v)| {
                        (
                            Key::String(Arc::new(k.to_owned())),
                            Value::String(Arc::new(v)),
                        )
                    })
                    .collect(),
                ),
            }));
        }
    }
    empty_condition()
}

#[allow(clippy::unnecessary_wraps)] // every condition_* arm answers with the same shape
fn empty_condition() -> ResolveResult {
    Ok(Value::Map(Map {
        map: Arc::new(
            ["type", "status", "reason", "message"]
                .into_iter()
                .map(|k| {
                    (
                        Key::String(Arc::new(k.to_owned())),
                        Value::String(Arc::new(String::new())),
                    )
                })
                .collect(),
        ),
    }))
}

/// The first `limit` (or all) non-overlapping matches of `re` in `s`.
fn regex_hits(
    s: &str,
    re: &str,
    limit: Option<i64>,
    f: &str,
) -> Result<Vec<String>, ExecutionError> {
    let re = regex::Regex::new(re)
        .map_err(|e| cel::ExecutionError::function_error(f, format!("invalid regex: {e}")))?;
    Ok(match limit {
        Some(n) if n <= 0 => Vec::new(),
        Some(n) => re
            .find_iter(s)
            .take(usize::try_from(n).unwrap_or(usize::MAX))
            .map(|m| m.as_str().to_owned())
            .collect(),
        None => re.find_iter(s).map(|m| m.as_str().to_owned()).collect(),
    })
}

/// Parse a URL for the k8s URL library: an absolute URI, or an absolute
/// path (parsed against a synthetic host the getters then hide — the
/// scheme/host/port read as empty and only the path survives).
fn parsed_url(s: &str) -> Result<UrlValue, String> {
    if s.starts_with('/') {
        let url = url::Url::parse(&format!("http://kapi.invalid{s}"))
            .map_err(|e| format!("invalid URL: {e}"))?;
        Ok(UrlValue {
            url,
            synthetic: true,
        })
    } else {
        let url = url::Url::parse(s).map_err(|e| format!("invalid URL: {e}"))?;
        Ok(UrlValue {
            url,
            synthetic: false,
        })
    }
}

/// Which getter a registered URL member resolves to.
#[derive(Clone, Copy)]
enum UrlGet {
    Scheme,
    Host,
    Hostname,
    Port,
    Path,
}

/// The parsed-URL opaque: the getters read [`url::Url`] fields.
#[derive(PartialEq, Eq)]
struct UrlValue {
    url: url::Url,
    /// An absolute-path URL: scheme/host/port read as empty.
    synthetic: bool,
}

impl UrlValue {
    fn pick(&self, get: UrlGet) -> String {
        let u = &self.url;
        if self.synthetic && !matches!(get, UrlGet::Path) {
            return String::new();
        }
        match get {
            UrlGet::Scheme => u.scheme().to_owned(),
            UrlGet::Host => match (u.host_str(), u.port()) {
                (Some(h), Some(p)) => format!("{h}:{p}"),
                (Some(h), None) => h.to_owned(),
                (None, Some(p)) => format!(":{p}"),
                (None, None) => String::new(),
            },
            UrlGet::Hostname => u.host_str().unwrap_or_default().to_owned(),
            UrlGet::Port => u.port().map_or_else(String::new, |p| p.to_string()),
            UrlGet::Path => u.path().to_owned(),
        }
    }
}

impl Opaque for UrlValue {
    fn runtime_type_name(&self) -> &'static str {
        "kubernetes.URL"
    }
}

impl std::fmt::Debug for UrlValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("UrlValue").field(&self.url.as_str()).finish()
    }
}

fn qty_of(v: &Value, f: &str) -> Result<crate::quantity::Quantity, ExecutionError> {
    match v {
        Value::Opaque(o) => o
            .downcast_ref::<crate::quantity::Quantity>()
            .copied()
            .ok_or_else(|| {
                cel::ExecutionError::function_error(f, format!("{f} needs a quantity() value"))
            }),
        _ => Err(cel::ExecutionError::function_error(
            f,
            format!("{f} needs a quantity() value"),
        )),
    }
}

fn qty_op(
    a: &Value,
    b: &Value,
    f: &str,
    op: fn(i128, i128) -> Option<i128>,
) -> Result<crate::quantity::Quantity, ExecutionError> {
    let (qa, qb) = (qty_of(a, f)?, qty_of(b, f)?);
    qa.binop(&qb, op)
        .map_err(|e| cel::ExecutionError::function_error(f, e))
}

fn qty_cmp(a: &Value, b: &Value, f: &str) -> Result<std::cmp::Ordering, ExecutionError> {
    let (qa, qb) = (qty_of(a, f)?, qty_of(b, f)?);
    qa.compare(&qb)
        .map_err(|e| cel::ExecutionError::function_error(f, e))
}

/// Build one evaluation context: the shared environment, the scope's
/// variables (converted through the crate's JSON mapping — the serde
/// converter would hand CEL `UInts` for JSON integers, and the spec
/// refuses `UInt * Int` arithmetic), and the kro + k8s function library.
#[must_use]
pub fn context(scope: &Scope) -> Context<'static, 'static> {
    let mut ctx = Context::with_env(env());
    for (name, v) in scope {
        let _ = ctx.add_variable(name.clone(), json_to_cel(v));
    }
    register_functions(&mut ctx);
    ctx
}

/// Compile + evaluate one expression to the raw CEL value (the omit
/// sentinel survives as the opaque marker).
///
/// # Errors
/// Parse or execution failure.
pub fn eval_program(expr: &str, scope: &Scope) -> Result<Value, String> {
    let program = program(expr)?;
    let ctx = context(scope);
    program.execute(&ctx).map_err(|e| format!("{e}"))
}

/// Compile + evaluate one expression to the JSON-level rendering.
///
/// # Errors
/// Parse, execution, or JSON conversion failure.
pub fn eval(expr: &str, scope: &Scope) -> Result<Rendered, String> {
    let v = eval_program(expr, scope)?;
    if is_omit(&v) {
        return Ok(Rendered::Omit);
    }
    Ok(Rendered::Value(cel_to_json(&v)?))
}

/// Compile + evaluate one expression, requiring a JSON value.
///
/// # Errors
/// As [`eval`], plus an omit sentinel where it is not allowed.
pub fn eval_value(expr: &str, scope: &Scope) -> Result<Json, String> {
    match eval(expr, scope)? {
        Rendered::Value(j) => Ok(j),
        Rendered::Omit => Err("omit() is not valid here".to_owned()),
    }
}

/// Evaluate one rendered string leaf. A whole-expression leaf takes the
/// CEL value (omit sentinel included); an interpolated leaf stringifies
/// each expression between its literals.
///
/// # Errors
/// Any expression failed to parse or evaluate.
pub fn eval_render(
    render: &Render,
    eval: &mut dyn FnMut(&str) -> Result<Value, String>,
) -> Result<Rendered, String> {
    match render {
        Render::Whole(e) => {
            let v = eval(e)?;
            if is_omit(&v) {
                return Ok(Rendered::Omit);
            }
            Ok(Rendered::Value(cel_to_json(&v)?))
        }
        Render::Parts(pieces) => {
            let mut out = String::new();
            for p in pieces {
                match p {
                    crate::expression::Piece::Literal(l) => out.push_str(l),
                    crate::expression::Piece::Expr(e) => {
                        let v = eval(e)?;
                        if is_omit(&v) {
                            return Ok(Rendered::Omit);
                        }
                        out.push_str(&stringify(&cel_to_json(&v)?));
                    }
                }
            }
            Ok(Rendered::Value(Json::String(out)))
        }
    }
}

/// One leaf's rendered answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Rendered {
    /// The field disappears.
    Omit,
    /// The field's value.
    Value(Json),
}

/// Stringify an interpolated value (the CEL `string()` conversion's
/// practical subset).
fn stringify(j: &Json) -> String {
    match j {
        Json::String(s) => s.clone(),
        Json::Bool(b) => b.to_string(),
        Json::Number(n) => n.to_string(),
        Json::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expression::classify;
    use serde_json::json;

    fn eval(expr: &str, scope: &Scope) -> Result<Value, String> {
        eval_program(expr, scope)
    }

    fn render_value(s: &str, scope: &Scope) -> Result<Json, String> {
        match &eval_render(&classify(s)?, &mut |e| eval(e, scope))? {
            Rendered::Value(j) => Ok(j.clone()),
            Rendered::Omit => Err("unexpected omit".to_owned()),
        }
    }

    fn base_scope() -> Scope {
        let mut scope = Scope::new();
        scope.insert(
            "schema".to_owned(),
            json!({"spec": {"name": "web", "replicas": 3, "items": ["b", "a", "c"],
                             "m": {"x": 1, "y": 2}, "uid": "u-1", "mem": "1Gi"}}),
        );
        scope.insert(
            "deployment".to_owned(),
            json!({"status": {"replicas": 1, "conditions": [
                {"type": "Available", "status": "True", "reason": "MinimumReplicasAvailable"}]}}),
        );
        scope
    }

    #[test]
    fn scope_variables_evaluate() {
        let scope = base_scope();
        assert_eq!(
            render_value("${schema.spec.name}", &scope).unwrap(),
            json!("web")
        );
        assert_eq!(
            render_value("web-${schema.spec.name}-v${schema.spec.replicas}", &scope).unwrap(),
            json!("web-web-v3")
        );
    }

    #[test]
    fn merge_shallow_and_deep_in_both_call_forms() {
        let mut scope = Scope::new();
        scope.insert("a".to_owned(), json!({"x": 1, "y": {"a": 1, "b": 1}}));
        scope.insert("b".to_owned(), json!({"y": {"b": 2, "c": 3}, "z": 4}));
        assert_eq!(
            render_value("${merge(a, b)}", &scope).unwrap(),
            json!({"x": 1, "y": {"b": 2, "c": 3}, "z": 4})
        );
        assert_eq!(
            render_value("${a.merge(b)}", &scope).unwrap(),
            json!({"x": 1, "y": {"b": 2, "c": 3}, "z": 4})
        );
        assert_eq!(
            render_value("${deepMerge(a, b)}", &scope).unwrap(),
            json!({"x": 1, "y": {"a": 1, "b": 2, "c": 3}, "z": 4})
        );
        assert_eq!(
            render_value("${a.deepMerge(b)}", &scope).unwrap(),
            json!({"x": 1, "y": {"a": 1, "b": 2, "c": 3}, "z": 4})
        );
    }

    #[test]
    fn non_map_merge_refuses() {
        let mut scope = Scope::new();
        scope.insert("a".to_owned(), json!(1));
        scope.insert("b".to_owned(), json!({"x": 1}));
        assert!(render_value("${merge(a, b)}", &scope).is_err());
    }

    #[test]
    fn omit_drops_the_field() {
        let mut scope = Scope::new();
        scope.insert("m".to_owned(), json!({"a": 1}));
        let out = eval_render(&classify("${m.a == 1 ? m.a : omit()}").unwrap(), &mut |e| {
            eval(e, &scope)
        })
        .unwrap();
        assert_eq!(out, Rendered::Value(json!(1)));
        let out = eval_render(&classify("${m.a == 2 ? m.a : omit()}").unwrap(), &mut |e| {
            eval(e, &scope)
        })
        .unwrap();
        assert_eq!(out, Rendered::Omit);
        assert!(
            render_value("${omit(m, \"a\")}", &scope).is_err(),
            "no key-filtering form upstream"
        );
    }

    #[test]
    fn hashes_return_bytes_and_encode() {
        let scope = base_scope();
        let sha = render_value("${base64.encode(hash.sha256(\"abc\"))}", &scope).unwrap();
        assert_eq!(sha, json!("ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="));
        let fnv = render_value("${base64.encode(hash.fnv64a(\"abc\"))}", &scope).unwrap();
        assert_eq!(fnv, json!("5x+iGQVBV0s="));
        let md5 = render_value("${base64.encode(hash.md5(\"abc\"))}", &scope).unwrap();
        // the digest rides as raw bytes: 16 bytes → 24 base64 chars
        assert_eq!(md5.as_str().unwrap_or_default().len(), 24);
    }

    #[test]
    fn deterministic_random_matches_upstream_signatures() {
        let mut scope = Scope::new();
        scope.insert("uid".to_owned(), json!("seed-1"));
        let a = render_value("${random.seededString(10, uid)}", &scope).unwrap();
        let b = render_value("${random.seededString(10, uid)}", &scope).unwrap();
        assert_eq!(a, b, "same seed, same string");
        let s = a.as_str().unwrap_or_default();
        assert_eq!(s.len(), 10);
        assert!(
            s.chars()
                .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()),
            "{s} rides the upstream alphabet"
        );
        let n = render_value("${random.seededInt(30000, 32768, uid)}", &scope).unwrap();
        let port = n.as_i64().unwrap_or_default();
        assert!((30000..32768).contains(&port), "{port} in [min, max)");
        assert!(render_value("${random.seededInt(5, 5, uid)}", &scope).is_err());
    }

    #[test]
    fn json_round_trip() {
        let scope = base_scope();
        let s = render_value("${json.marshal({\"x\": 1})}", &scope).unwrap();
        assert_eq!(s, json!(r#"{"x":1}"#));
        let back = render_value("${json.unmarshal('{\"k\": [1, 2]}')}", &scope).unwrap();
        assert_eq!(back, json!({"k": [1, 2]}));
    }

    #[test]
    fn optional_syntax_and_or_value() {
        let scope = base_scope();
        assert_eq!(
            render_value("${deployment.status.?replicas.orValue(0)}", &scope).unwrap(),
            json!(1)
        );
        assert_eq!(
            render_value("${deployment.status.?missing.orValue(0)}", &scope).unwrap(),
            json!(0)
        );
        assert_eq!(
            render_value("${deployment.status.?replicas.orValue(0) > 0}", &scope).unwrap(),
            json!(true)
        );
    }

    #[test]
    fn string_and_list_extensions_work() {
        let scope = base_scope();
        assert_eq!(
            render_value("${\"a-b-c\".split(\"-\")}", &scope).unwrap(),
            json!(["a", "b", "c"])
        );
        assert_eq!(
            render_value("${schema.spec.items.join(\",\")}", &scope).unwrap(),
            json!("b,a,c")
        );
        assert_eq!(
            render_value("${schema.spec.items.sort()}", &scope).unwrap(),
            json!(["a", "b", "c"])
        );
        assert_eq!(
            render_value("${schema.spec.items.distinct()}", &scope).unwrap(),
            json!(["b", "a", "c"])
        );
        assert_eq!(
            render_value("${schema.spec.items.slice(0, 2)}", &scope).unwrap(),
            json!(["b", "a"])
        );
        assert_eq!(
            render_value("${lists.range(3)}", &scope).unwrap(),
            json!([0, 1, 2])
        );
        assert_eq!(
            render_value("${\"%x\".format([255])}", &scope).unwrap(),
            json!("ff")
        );
        assert_eq!(
            render_value("${\"  x \".trim()}", &scope).unwrap(),
            json!("x")
        );
        assert_eq!(
            render_value("${\"AbC\".lowerAscii()}", &scope).unwrap(),
            json!("abc")
        );
        assert_eq!(
            render_value("${\"abc\".substring(1)}", &scope).unwrap(),
            json!("bc")
        );
    }

    #[test]
    fn two_variable_comprehensions_work() {
        let scope = base_scope();
        // Map iteration order is not stable (upstream kro documents this
        // and tells authors to sortBy), so compare sorted.
        assert_eq!(
            render_value(
                "${schema.spec.m.transformList(k, v, k).sortBy(i, i)}",
                &scope
            )
            .unwrap(),
            json!(["x", "y"])
        );
        assert_eq!(
            render_value("${schema.spec.m.transformMap(k, v, v * 2)}", &scope).unwrap(),
            json!({"x": 2, "y": 4})
        );
        assert_eq!(
            render_value("${schema.spec.m.all(k, v, v > 0)}", &scope).unwrap(),
            json!(true)
        );
        assert_eq!(
            render_value("${schema.spec.items.sortBy(i, i)}", &scope).unwrap(),
            json!(["a", "b", "c"])
        );
        assert_eq!(
            render_value("${cel.bind(x, 2, x * 3)}", &scope).unwrap(),
            json!(6)
        );
    }

    #[test]
    fn index_mutations_hold_upstream_bounds() {
        let scope = base_scope();
        assert_eq!(
            render_value("${lists.setAtIndex([1, 2, 3], 1, 99)}", &scope).unwrap(),
            json!([1, 99, 3])
        );
        assert_eq!(
            render_value("${lists.insertAtIndex([1, 2, 3], 0, 99)}", &scope).unwrap(),
            json!([99, 1, 2, 3])
        );
        assert_eq!(
            render_value("${lists.insertAtIndex([1, 2, 3], 3, 99)}", &scope).unwrap(),
            json!([1, 2, 3, 99])
        );
        assert_eq!(
            render_value("${lists.removeAtIndex([1, 2, 3], 1)}", &scope).unwrap(),
            json!([1, 3])
        );
        assert!(render_value("${lists.setAtIndex([1, 2, 3], 3, 99)}", &scope).is_err());
        assert!(render_value("${lists.removeAtIndex([1, 2, 3], 3)}", &scope).is_err());
    }

    #[test]
    fn new_condition_builds_and_validates() {
        let scope = Scope::new();
        let out = render_value(
            "${runtime.newCondition({type: 'Deployed', status: 'True', reason: 'Ready', message: 'all good'})}",
            &scope,
        )
        .unwrap();
        assert_eq!(
            out,
            json!({"type": "Deployed", "status": "True", "reason": "Ready", "message": "all good"})
        );
        // bad key refuses at parse time
        assert!(
            env()
                .compile("runtime.newCondition({type: 'X', bogus: 'Y'})")
                .is_err()
        );
        // missing type refuses at parse time
        assert!(
            env()
                .compile("runtime.newCondition({status: 'True'})")
                .is_err()
        );
        // quoted keys refuse at parse time
        assert!(
            env()
                .compile("runtime.newCondition({'type': 'X', 'status': 'True'})")
                .is_err()
        );
        // bad status refuses at evaluation
        assert!(eval("runtime.newCondition({type: 'X', status: 'Bogus'})", &scope).is_err());
    }

    #[test]
    fn condition_reads_the_builtins() {
        let scope = base_scope();
        let out = render_value(
            "${runtime.condition(deployment, 'Available').status}",
            &scope,
        )
        .unwrap();
        assert_eq!(out, json!("True"));
        let out = render_value(
            "${runtime.condition(deployment, 'ResourcesReady').status}",
            &scope,
        )
        .unwrap();
        assert_eq!(
            out,
            json!(""),
            "a missing condition yields the empty condition"
        );
    }

    #[test]
    fn regex_find_and_find_all() {
        let scope = Scope::new();
        assert_eq!(
            render_value(
                "${\"v1.2.3 and v4.5.6\".findAll('[0-9]+\\\\.[0-9]+')}",
                &scope
            )
            .unwrap(),
            json!(["1.2", "4.5"])
        );
        assert_eq!(
            render_value("${\"v1.2.3\".find('[0-9]+')}", &scope).unwrap(),
            json!("1")
        );
        assert_eq!(
            render_value("${\"a b a\".findAll('a', 1)}", &scope).unwrap(),
            json!(["a"])
        );
    }

    #[test]
    fn url_family_parses_and_reads() {
        let scope = Scope::new();
        assert_eq!(
            render_value("${isURL('https://a.b:8080/c?x=1')}", &scope).unwrap(),
            json!(true)
        );
        assert_eq!(
            render_value("${isURL('not a url')}", &scope).unwrap(),
            json!(false)
        );
        assert_eq!(
            render_value("${url('https://a.b:8080/c?x=1').getHost()}", &scope).unwrap(),
            json!("a.b:8080")
        );
        assert_eq!(
            render_value("${url('https://a.b:8080/c?x=1').getHostname()}", &scope).unwrap(),
            json!("a.b")
        );
        assert_eq!(
            render_value("${url('https://a.b:8080/c?x=1').getPort()}", &scope).unwrap(),
            json!("8080")
        );
        assert_eq!(
            render_value("${url('https://a.b:8080/c?x=1').getScheme()}", &scope).unwrap(),
            json!("https")
        );
        assert_eq!(
            render_value("${url('https://a.b:8080/c?x=1').getEscapedPath()}", &scope).unwrap(),
            json!("/c")
        );
        assert_eq!(
            render_value(
                "${url('https://a.b/c?x=1&x=2&y=3').getQuery()['x'][1]}",
                &scope
            )
            .unwrap(),
            json!("2")
        );
    }

    #[test]
    fn quantities_parse_and_arithmetize() {
        let scope = base_scope();
        assert_eq!(
            render_value("${isQuantity('1Gi')}", &scope).unwrap(),
            json!(true)
        );
        assert_eq!(
            render_value("${isQuantity('bogus')}", &scope).unwrap(),
            json!(false)
        );
        assert_eq!(
            render_value("${quantity('1Gi').asInteger()}", &scope).unwrap(),
            json!(1_073_741_824)
        );
        assert_eq!(
            render_value("${quantity('100m').asApproximateFloat()}", &scope).unwrap(),
            json!(0.1)
        );
        assert_eq!(
            render_value(
                "${quantity('2Gi').sub(quantity(schema.spec.mem)).asInteger()}",
                &scope
            )
            .unwrap(),
            json!(1_073_741_824)
        );
        assert_eq!(
            render_value(
                "${quantity('100m').add(quantity('500m')).asApproximateFloat()}",
                &scope
            )
            .unwrap(),
            json!(0.6)
        );
        assert_eq!(
            render_value("${quantity('512Mi').isLessThan(quantity('1Gi'))}", &scope).unwrap(),
            json!(true)
        );
        assert_eq!(
            render_value("${quantity('2Gi').isGreaterThan(quantity('1Gi'))}", &scope).unwrap(),
            json!(true)
        );
        assert_eq!(
            render_value("${quantity('1Gi').compareTo(quantity('1Gi'))}", &scope).unwrap(),
            json!(0)
        );
        assert_eq!(
            render_value("${quantity('-3').sign()}", &scope).unwrap(),
            json!(-1)
        );
        assert_eq!(
            render_value("${quantity('0.5').isInteger()}", &scope).unwrap(),
            json!(false)
        );
    }

    #[test]
    fn unknown_identifier_is_an_eval_error() {
        let scope = base_scope();
        assert!(render_value("${nothere.field}", &scope).is_err());
    }
}
