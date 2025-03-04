FROM --platform=$BUILDPLATFORM docker.io/library/rust:1.83-slim-bullseye AS build
ENV CARGO_INCREMENTAL=0
ARG TARGETARCH="amd64"
WORKDIR /src
COPY . .
RUN cargo fetch --locked
RUN cargo build --frozen --release

FROM docker.io/library/debian:bullseye-slim
COPY --from=build /src/target/release/schlep /usr/local/bin/schlep
ENTRYPOINT ["/usr/local/bin/schlep"]
