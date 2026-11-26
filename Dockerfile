# The krapi controller binary as a scratch image. CI builds a static
# x86_64 musl executable and stages it at build/krapi before docker build.
FROM scratch
COPY build/krapi /krapi
ENTRYPOINT ["/krapi"]
CMD ["run"]
