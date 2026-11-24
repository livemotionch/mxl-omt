# syntax=docker/dockerfile:1
# lm-mxl-omt: MXL ⇄ Open Media Transport bridge.
#   docker build -f docker/Dockerfile -t lm-mxl-omt:dev .
# MXL pin: dmf-mxl/mxl release/v1.1 218ddaa0a08c12ffe75fc475ae65aa3d9eef16d7, fabrics off
# (the mxl-stack functions' pin).
# UBUNTU: the base image (e.g. public.ecr.aws/docker/library/ubuntu:24.04 when
# Docker Hub rate-limits). OMT: libomt + libomtnet (.NET 8 NativeAOT) and
# libvmx (C++), all MIT, from github.com/openmediatransport at the commits below.

ARG UBUNTU=ubuntu:24.04

FROM ${UBUNTU} AS mxl
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential cmake ninja-build git curl ca-certificates zip unzip tar pkg-config uuid-dev python3 \
    && rm -rf /var/lib/apt/lists/*
COPY scripts/build-mxl.sh /usr/local/bin/build-mxl.sh
RUN VCPKG_ROOT=/opt/vcpkg MXL_BUILD_DIR=/tmp/mxl /usr/local/bin/build-mxl.sh /opt/mxl \
    && rm -rf /tmp/mxl /opt/vcpkg/buildtrees /opt/vcpkg/downloads

# libomt is libomtnet (C#) compiled to a native shared library with NativeAOT.
FROM mcr.microsoft.com/dotnet/sdk:8.0-noble AS omt
ARG LIBOMT_REF=bb4b67be8e5964f1017592b9fc5406495e704e5d
ARG LIBOMTNET_REF=7d29031ff3f355ffa1f0cd08ee24d02e258445dd
ARG LIBVMX_REF=31724bb96530325a989bc157c8e07538d6f1c4db
ARG EXAMPLES_REF=8963efe0ce48f288c231fbedd76264144eea8d4a
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        clang lld zlib1g-dev git ca-certificates g++ \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /omt
RUN for r in libomt:$LIBOMT_REF libomtnet:$LIBOMTNET_REF libvmx:$LIBVMX_REF Examples:$EXAMPLES_REF; do \
        n=${r%%:*}; ref=${r#*:}; git init -q $n && git -C $n remote add origin https://github.com/openmediatransport/$n \
        && git -C $n fetch -q --depth 1 origin $ref && git -C $n checkout -q FETCH_HEAD; done
# libvmx: its Linux x64 recipe (build/buildlinuxx64.sh), AVX2.
RUN cd libvmx/build && clang++ -O3 -std=c++17 -fdeclspec -fPIC -mlzcnt -mavx2 -mbmi -shared -Wl,-rpath,'$ORIGIN' \
        ../src/vmxcodec_x86.cpp ../src/vmxcodec_avx2.cpp ../src/vmxcodec.cpp -o libvmx.so
RUN --mount=type=cache,target=/root/.nuget/packages \
    cd libomtnet && dotnet build -c Release libomtnet.csproj \
    && cd ../libomt && dotnet publish libomt.csproj -r linux-x64 -c Release \
    && mkdir -p /out/lib /out/include /out/bin /out/share \
    && cp bin/Release/net8.0/linux-x64/native/libomt.so /out/lib/ \
    && cp libomt.h /out/include/ && cp ../libvmx/build/libvmx.so /out/lib/
# libomt's C++ examples (omtsendtest, omtrecvtest): an independent OMT
# sender and receiver for interop tests, headless.
RUN cd Examples/C++ \
    && g++ -O2 -std=c++17 -I/out/include omtsendtest/omtsendtest.cpp -L/out/lib -lomt -Wl,-rpath,/usr/local/lib -o /out/bin/omtsendtest \
    && g++ -O2 -std=c++17 -I/out/include omtrecvtest/omtrecvtest.cpp -L/out/lib -lomt -Wl,-rpath,/usr/local/lib -o /out/bin/omtrecvtest \
    && cp omtsendtest/california-1080-uyvy.yuv /out/share/ \
    && for f in libomt libomtnet libvmx Examples; do cp /omt/$f/LICENSE* /out/share/LICENSE.$f.txt; done

FROM ${UBUNTU} AS build
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential curl ca-certificates git pkg-config clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.98.1
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY web ./web
RUN --mount=type=cache,target=/opt/cargo/registry --mount=type=cache,target=/opt/cargo/git \
    cargo build --release --locked --bins \
    && install -m 0755 target/release/lm-mxl-omt /usr/local/bin/

FROM ${UBUNTU} AS runtime
ARG MXL_REF=218ddaa0a08c12ffe75fc475ae65aa3d9eef16d7
ARG VERSION=dev
# libicu: .NET globalisation in libomt. libavahi-client: OMT discovery
# (DNS-SD) on Linux; dbus + avahi-daemon run discovery inside the container
# (OMT_AVAHI=internal, docker/entrypoint.sh).
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates libstdc++6 libicu74 libavahi-client3 avahi-daemon dbus util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /config && chown 1000:1000 /config \
    && sed -i -e 's/^#*\s*enable-dbus=.*/enable-dbus=yes/' -e '/^rlimit-/d' /etc/avahi/avahi-daemon.conf
COPY --from=mxl /opt/mxl/lib/libmxl.so* /usr/local/lib/
COPY --from=omt /out/lib/ /usr/local/lib/
COPY --from=omt /out/include/ /usr/local/include/
COPY --from=omt /out/bin/ /usr/local/bin/
COPY --from=omt /out/share/ /usr/local/share/omt/
COPY --from=build /usr/local/bin/lm-mxl-omt /usr/local/bin/
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN ldconfig && ldd /usr/local/lib/libmxl.so.1 | (! grep -q "not found") \
    && ldd /usr/local/lib/libomt.so | (! grep -q "not found") \
    && ldd /usr/local/bin/lm-mxl-omt | (! grep -q "not found")
ENV OMT_STATE_DIR=/config OMT_STORAGE_PATH=/config/omt
# Web/API/metrics, NMOS; OMT senders listen from 6400 (OMT_PORT_START).
EXPOSE 8200 3334
LABEL org.opencontainers.image.title="lm-mxl-omt" \
      org.opencontainers.image.description="MXL <-> Open Media Transport bridge (Livemotion)" \
      org.opencontainers.image.version="${VERSION}" \
      io.dmf.mxl.revision="${MXL_REF}"
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["/usr/local/bin/lm-mxl-omt"]
