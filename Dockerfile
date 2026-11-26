# The krapi controller binary as a scratch image. CI builds a static
# x86_64 musl executable and stages it at build/krapi before docker build.
FROM scratch
COPY --from=alpine:3.20 /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY build/krapi /krapi
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
ENTRYPOINT ["/krapi"]
CMD ["run"]
