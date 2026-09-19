# Image for regenerating vhs/demo.gif.
#
# Native vhs is unreliable on Windows (it hangs on `Set Theme`), and the
# tape needs a throwaway jj repo with a known history anyway — a
# container gives both a working vhs and a reproducible repo, so the GIF
# does not depend on whatever state the author's machine is in.
#
# Build (from this directory):
#   docker build -t shikigami-vhs .
#
# Run (regenerate demo.gif into the current dir):
#   docker run --rm -v "$PWD:/vhs" shikigami-vhs demo.tape
#
# Or `bash regen.sh` once the image exists.

# Pinned, NOT `:latest`: the v0.12.0 image renders nothing. It logs
# "Creating demo.gif...", exits 0, and writes no file — verified on the
# pristine upstream image (`ghcr.io/charmbracelet/vhs:latest`, digest
# sha256:b1afb4f…, 2026-09-19) with a three-line tape, so it is not
# something this Dockerfile or the tape does. v0.11.0 produces the GIF.
# Re-test before bumping.
# renovate: datasource=docker depName=ghcr.io/charmbracelet/vhs
FROM ghcr.io/charmbracelet/vhs:v0.11.0

# git for `jj git init`; the rest is the build toolchain for
# `cargo install shikigami`.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        unzip \
        fontconfig \
        build-essential \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

# PlemolJP Console NF — the font demo.tape selects. Without it vhs falls
# back to a generic family and the box-drawing characters jj uses for the
# change graph (│ ○ ◆ ├─╮) render as tofu, which is precisely the part of
# the UI the GIF exists to show.
ARG PLEMOLJP_VERSION=v3.0.0
RUN curl -fsSL -o /tmp/plemoljp_nf.zip \
        "https://github.com/yuru7/PlemolJP/releases/download/${PLEMOLJP_VERSION}/PlemolJP_NF_${PLEMOLJP_VERSION}.zip" \
    && mkdir -p /usr/share/fonts/truetype/plemoljp \
    && unzip -j /tmp/plemoljp_nf.zip "*.ttf" -d /usr/share/fonts/truetype/plemoljp \
    && fc-cache -f \
    && rm -f /tmp/plemoljp_nf.zip

# jj itself — the release binary rather than `cargo install jj-cli`,
# which would add several minutes of compile time to the image build.
ARG JJ_VERSION=v0.45.1
RUN curl -fsSL -o /tmp/jj.tar.gz \
        "https://github.com/jj-vcs/jj/releases/download/${JJ_VERSION}/jj-${JJ_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    && mkdir -p /tmp/jj-dist \
    && tar xzf /tmp/jj.tar.gz -C /tmp/jj-dist \
    && install -m 0755 /tmp/jj-dist/jj /usr/local/bin/jj \
    && rm -rf /tmp/jj.tar.gz /tmp/jj-dist

# Rust (minimal profile: rustc + cargo only).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

# shikigami — pinned so an old image does not silently lag the release
# the GIF claims to show. Bump alongside each release tag.
ARG SHIKIGAMI_VERSION=0.0.2
RUN cargo install shikigami --version "${SHIKIGAMI_VERSION}" --locked

# jj refuses to create commits without an author, and the demo repo is
# built inside the container, so configure a user globally.
RUN jj config set --user user.name "yukimemi" \
    && jj config set --user user.email "yukimemi@gmail.com"

# Default working dir; bind-mount the host vhs/ here at run time.
WORKDIR /vhs
