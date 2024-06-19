FROM docker.io/library/rust:1.76-slim-bullseye as build
WORKDIR /src
COPY . .
RUN cargo fetch --locked
RUN cargo build --frozen --release

FROM docker.io/library/debian:bullseye-slim
COPY --from=build /src/target/release/schlep /usr/local/bin/schlep
ENTRYPOINT ["/usr/local/bin/schlep"]
