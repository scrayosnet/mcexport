FROM rust:alpine@sha256:466dc9924d265455aa73e72fd9cdac9db69ce6a988e6f0e6baf852db3485d97d AS builder

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
