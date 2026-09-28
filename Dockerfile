# Built on each platform itself (QEMU for arm64 in CI), since ring's C is
# the one part that will not cross-compile without a C toolchain for the
# target. Rust links a musl binary statically, so it runs on distroless's
# static base, whose CA roots are how it reaches upstream and the bucket.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
ARG VERSION=dev
ENV RELEASEWAY_VERSION=$VERSION
WORKDIR /src
# The dependencies first, in a layer of their own, so a change to src/
# does not build them again.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked && rm -rf src
COPY src src
RUN touch src/main.rs && cargo build --release --locked

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/releaseway /releaseway
ENV LISTEN_ADDR=0.0.0.0:8080
EXPOSE 8080
USER nonroot:nonroot
ENTRYPOINT ["/releaseway"]
