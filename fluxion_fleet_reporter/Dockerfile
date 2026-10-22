ARG BUILD_FROM
FROM ${BUILD_FROM}

RUN apk add --no-cache nodejs

COPY rootfs /

CMD ["node","/app/reporter.mjs"]
