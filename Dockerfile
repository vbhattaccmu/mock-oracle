FROM rust:1.72-alpine AS build

RUN apk add --update alpine-sdk

WORKDIR /src
COPY . /src

RUN cargo build --release

FROM alpine:latest

COPY --from=build /src/target/release/dev-oracle /usr/bin/

ENTRYPOINT [ "dev-oracle" ]
