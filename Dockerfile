FROM rust:alpine@sha256:4fec02de605563c297c78a31064c8335bc004fa2b0bf406b1b99441da64e2d2d AS builder

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
