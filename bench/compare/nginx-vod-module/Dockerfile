# nginx with Kaltura's nginx-vod-module, built from the upstream release tarballs.
#
# Only used by the comparison benchmark in bench/compare. nginx-vod-module is AGPL-3.0; nothing
# here copies or links its source into segmentor. It is downloaded at build time and run as a
# separate program, as any operator would run it.
ARG DEBIAN_IMAGE=debian:bookworm-slim

FROM ${DEBIAN_IMAGE} AS build
ARG NGINX_VERSION=1.26.3
ARG VOD_VERSION=1.33
RUN apt-get update \
 && apt-get install --yes --no-install-recommends \
      build-essential ca-certificates curl libpcre3-dev libssl-dev zlib1g-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /build
RUN curl -fsSL "https://nginx.org/download/nginx-${NGINX_VERSION}.tar.gz" | tar xz \
 && curl -fsSL "https://github.com/kaltura/nginx-vod-module/archive/refs/tags/${VOD_VERSION}.tar.gz" | tar xz
WORKDIR /build/nginx-${NGINX_VERSION}
# The module's own recommended build options: file AIO, thread pools, -O3.
RUN ./configure \
      --prefix=/usr/local/nginx \
      --with-file-aio \
      --with-threads \
      --with-http_stub_status_module \
      --with-cc-opt="-O3 -mpopcnt" \
      --add-module=/build/nginx-vod-module-${VOD_VERSION} \
 && make -j"$(nproc)" \
 && make install

FROM ${DEBIAN_IMAGE}
RUN apt-get update \
 && apt-get install --yes --no-install-recommends libpcre3 libssl3 zlib1g \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/nginx /usr/local/nginx
COPY nginx.conf response-cache.conf /usr/local/nginx/conf/
EXPOSE 80
STOPSIGNAL SIGQUIT
CMD ["/usr/local/nginx/sbin/nginx", "-g", "daemon off;"]
