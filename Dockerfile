# Cross-compile diskclone for Windows (x86_64) from Linux/macOS
#
# Build:  docker build -t diskclone-builder .
# Run:    docker run --rm -v $(pwd)/target:/app/target -v $(pwd):/app/src:ro diskclone-builder
#         (binary ends up in ./target/x86_64-pc-windows-gnu/release/diskclone.exe)
#
# Or one-liner: docker run --rm -v $(pwd):/app -w /app diskclone-builder cargo build --release --target x86_64-pc-windows-gnu

FROM rust:1.86-bookworm

# Install MinGW cross-compiler, git (for versioning), and build deps for libssh2
RUN apt-get update && apt-get install -y --no-install-recommends \
    g++-mingw-w64-x86-64 \
    gcc-mingw-w64-x86-64 \
    cmake \
    pkg-config \
    git \
    && rm -rf /var/lib/apt/lists/*

# Add Windows target and configure linker
RUN rustup target add x86_64-pc-windows-gnu

ENV CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc

WORKDIR /app

# Default: build and show output path
CMD ["cargo", "build", "--release", "--target", "x86_64-pc-windows-gnu"]
