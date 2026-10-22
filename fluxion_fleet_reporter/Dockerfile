ARG BUILD_FROM
FROM ${BUILD_FROM}

RUN apk add --no-cache nodejs

COPY rootfs /

# Run node as the entrypoint, not under the base image's s6 init: s6 starts the
# command without the container environment, which drops SUPERVISOR_TOKEN.
ENTRYPOINT ["node","/app/reporter.mjs"]
