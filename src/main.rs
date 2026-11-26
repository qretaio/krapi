//! THE KRO CLI — direct, offline use of the engine's own laws: validate a
//! `ResourceGraphDefinition` and emit the CRD it synthesizes, without a
//! cluster. `kro run` keeps the standalone engine's old runner behavior.
//!
//! This mirrors upstream's `cmd/kro`: `validate rgd` and `generate crd`
//! run the SAME parse/compile pipeline the controller runs, so a file
//! accepted here is accepted by the engine.

use clap::{Parser, Subcommand};
use krapi::graph::{CompileError, EngineDefaults};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "krapi",
    about = "the kube resource orchestrator: validate RGDs, generate CRDs, run the engine"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Validate a `ResourceGraphDefinition` offline (the controller's law).
    Validate {
        /// Path to the RGD YAML/JSON file.
        #[arg(short, long, value_name = "FILE")]
        file: PathBuf,
        /// Group generated instance CRDs land in when the RGD omits one.
        #[arg(long, default_value = "kro.run", env = "KRO_INSTANCE_GROUP")]
        instance_group: String,
    },
    /// Generate an artifact from an RGD.
    Generate(GenerateArgs),
    /// Run the standalone engine against an API server.
    Run(RunArgs),
}

#[derive(clap::Args)]
struct GenerateArgs {
    #[command(subcommand)]
    what: GenerateCmd,
}

#[derive(Subcommand)]
enum GenerateCmd {
    /// Print the synthesized instance CRD.
    Crd {
        #[arg(short, long, value_name = "FILE")]
        file: PathBuf,
        /// Output format.
        #[arg(long, value_enum, default_value_t = Format::Yaml)]
        format: Format,
        #[arg(long, default_value = "kro.run", env = "KRO_INSTANCE_GROUP")]
        instance_group: String,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum Format {
    Yaml,
    Json,
}

#[derive(clap::Args)]
struct RunArgs {
    #[arg(long, default_value = "http://127.0.0.1:8643", env = "KRO_SERVER")]
    server: String,
    #[arg(long, env = "KRO_TOKEN")]
    token: Option<String>,
    #[arg(long, default_value = "default", env = "KRO_NAMESPACE")]
    namespace: String,
    #[arg(long, default_value = "kro.run", env = "KRO_RGD_GROUP")]
    rgd_group: String,
    #[arg(long, default_value = "v1alpha1", env = "KRO_RGD_VERSION")]
    rgd_version: String,
    #[arg(long, default_value = "kro.run", env = "KRO_INSTANCE_GROUP")]
    instance_group: String,
    #[arg(long, default_value_t = 4)]
    workers: usize,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match Cli::parse().cmd {
        Cmd::Validate {
            file,
            instance_group,
        } => validate(&file, &instance_group),
        Cmd::Generate(GenerateArgs {
            what:
                GenerateCmd::Crd {
                    file,
                    format,
                    instance_group,
                },
        }) => generate_crd(&file, format, &instance_group),
        Cmd::Run(args) => run(args),
    }
}

/// Read an RGD file as JSON (YAML is a superset handled by `serde_yaml`).
fn read_rgd(path: &PathBuf) -> anyhow::Result<serde_json::Value> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("parse {} as YAML: {e}", path.display()))?;
    serde_yaml::from_value(yaml)
        .map_err(|e| anyhow::anyhow!("{} is not a JSON-shaped object: {e}", path.display()))
}

/// Compile exactly as the controller does; returns the parsed RGD + graph.
fn compile(
    path: &PathBuf,
    instance_group: &str,
) -> anyhow::Result<(krapi::ResourceGraphDefinition, krapi::CompiledGraph)> {
    let body = read_rgd(path)?;
    let rgd = krapi::api::parse(&body, instance_group)
        .map_err(|e| anyhow::anyhow!("invalid ResourceGraphDefinition: {e}"))?;
    let defaults = EngineDefaults {
        instance_group: instance_group.to_owned(),
        ..EngineDefaults::default()
    };
    let graph = krapi::graph::compile(&rgd, &defaults).map_err(|e| match e {
        CompileError::Invalid(msg) => anyhow::anyhow!("invalid ResourceGraphDefinition: {msg}"),
        CompileError::Cycle(cycle) => anyhow::anyhow!("dependency cycle: {cycle}"),
    })?;
    Ok((rgd, graph))
}

fn validate(path: &PathBuf, instance_group: &str) -> anyhow::Result<()> {
    let (rgd, graph) = compile(path, instance_group)?;
    println!(
        "Validation successful: {} ({} nodes, topological order: {})",
        rgd.name,
        graph.nodes.len(),
        graph.topological_order.join(" -> ")
    );
    Ok(())
}

fn generate_crd(path: &PathBuf, format: Format, instance_group: &str) -> anyhow::Result<()> {
    let (_, graph) = compile(path, instance_group)?;
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&graph.instance_crd)?),
        Format::Yaml => println!("{}", serde_yaml::to_string(&graph.instance_crd)?),
    }
    Ok(())
}

fn run(args: RunArgs) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        use krapi::face::SharedFace;
        use krapi::{Engine, EngineConfig};

        let client = krapi_kube::connect(&args.server, args.token.as_deref(), &args.namespace)
            .map_err(|e| anyhow::anyhow!("the API server client failed: {e}"))?;
        tracing::info!(server = %args.server, "kro: joined the API server");
        let face = SharedFace::erase(krapi_kube::KubeFace::new(client));
        let config = EngineConfig {
            defaults: EngineDefaults {
                instance_group: args.instance_group.clone(),
                rgd_group: args.rgd_group.clone(),
                rgd_version: args.rgd_version.clone(),
            },
            workers: args.workers,
            ..EngineConfig::default()
        };
        if let Err(e) = krapi_kube::ensure_rgd_crd(&face, &args.rgd_group, &args.rgd_version).await
        {
            anyhow::bail!("the RGD CRD install failed: {e}");
        }
        Engine::new(face, config).run().await;
        Ok(())
    })
}
