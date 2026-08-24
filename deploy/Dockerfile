# A watchdog is the thing that has to still be running when everything else is
# not, so this image is deliberately dull: static binary, no shell, no package
# manager, nothing to keep patched.

FROM rust:1.96-slim-bookworm AS build

WORKDIR /src

# Dependencies first, so an edit to src/ does not rebuild the whole tree.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
# cargo skips a rebuild when mtimes look unchanged; the dummy above shares them.
RUN touch src/main.rs src/lib.rs \
 && cargo build --release --locked \
 && strip target/release/stillwatch


# `cc` rather than `static` because reqwest's TLS stack wants libc. Still no
# shell and no package manager in the final image.
FROM gcr.io/distroless/cc-debian12

COPY --from=build /src/target/release/stillwatch /usr/local/bin/stillwatch

# The receiver binds here; the monitored jobs post to it.
EXPOSE 9111

# Incidents are written under /var/lib/stillwatch. Mount a volume there or the
# audit trail goes with the container — and `report` will honestly say it has
# no record of the window rather than claiming everything was fine.
VOLUME ["/var/lib/stillwatch"]

# Secrets arrive as ${TELEGRAM_TOKEN} and ${TELEGRAM_CHAT}, or as
# STILLWATCH_NOTIFY_TELEGRAM_TOKEN / _CHAT_ID. Never bake them into the image.
ENV STILLWATCH_CONFIG=/etc/stillwatch/stillwatch.toml

USER nonroot:nonroot

ENTRYPOINT ["/usr/local/bin/stillwatch"]
