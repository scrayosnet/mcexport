FROM rust:alpine@sha256:9ab8f4eab808b1383c7e60a15fbf291e949fec85c3f98c34fb145b16c4ced0a1 AS builder

# specify our build directory
WORKDIR /usr/src/mcexport

# copy the source files into the engine
COPY . .

# install dev dependencies and perform build process
RUN set -eux \
 && apk add --no-cache musl-dev \
 && cargo build --release


FROM scratch

# declare our metrics port
EXPOSE 8080

# copy the raw binary into the new image
COPY --from=builder "/usr/src/mcexport/target/release/mcexport" "/mcexport"

# copy the users and groups for the nobody user and group
COPY --from=builder "/etc/passwd" "/etc/passwd"
COPY --from=builder "/etc/group" "/etc/group"

# we run with minimum permissions as the nobody user
USER nobody:nobody

# just execute the raw binary without any wrapper
ENTRYPOINT ["/mcexport"]
