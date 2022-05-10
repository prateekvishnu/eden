# Build Stage
FROM ubuntu:20.04 as builder

## Install build dependencies.
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y cmake clang curl
RUN curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
RUN ${HOME}/.cargo/bin/rustup default nightly
RUN ${HOME}/.cargo/bin/cargo install -f cargo-fuzz

## Add source code to the build stage.
ADD . /eden
WORKDIR /eden

RUN cd eden/scm/lib/dag/fuzz && ${HOME}/.cargo/bin/cargo fuzz build

# Package Stage
FROM ubuntu:20.04

COPY --from=builder eden/eden/scm/lib/dag/fuzz/target/x86_64-unknown-linux-gnu/release/gca /
COPY --from=builder eden/eden/scm/lib/dag/fuzz/target/x86_64-unknown-linux-gnu/release/gca_octopus /
COPY --from=builder eden/eden/scm/lib/dag/fuzz/target/x86_64-unknown-linux-gnu/release/gca_small /
COPY --from=builder eden/eden/scm/lib/dag/fuzz/target/x86_64-unknown-linux-gnu/release/range /
COPY --from=builder eden/eden/scm/lib/dag/fuzz/target/x86_64-unknown-linux-gnu/release/range_medium /
COPY --from=builder eden/eden/scm/lib/dag/fuzz/target/x86_64-unknown-linux-gnu/release/range_octopus /
COPY --from=builder eden/eden/scm/lib/dag/fuzz/target/x86_64-unknown-linux-gnu/release/range_small /



