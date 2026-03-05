# Multi-stage Dockerfile for testing advanced features
ARG BASE_IMAGE=alpine:latest
ARG APP_VERSION=1.0.0

# Build stage
FROM ${BASE_IMAGE} AS builder

ARG APP_VERSION
ARG BUILD_DATE

RUN apk add --no-cache gcc musl-dev make

WORKDIR /build

# Create a simple C program
RUN echo '#include <stdio.h>' > hello.c && \
    echo 'int main() {' >> hello.c && \
    echo '    printf("Version: '${APP_VERSION}'\\n");' >> hello.c && \
    echo '    printf("Built on: '${BUILD_DATE}'\\n");' >> hello.c && \
    echo '    return 0;' >> hello.c && \
    echo '}' >> hello.c

RUN gcc -o hello hello.c

# Final stage
FROM ${BASE_IMAGE} AS final

LABEL version="${APP_VERSION}"
LABEL build_date="${BUILD_DATE}"

WORKDIR /app

COPY --from=builder /build/hello /app/hello

ENTRYPOINT ["/app/hello"]

# Development stage (for testing target selection)
FROM final AS dev

RUN apk add --no-cache bash vim

ENTRYPOINT ["/bin/bash"]
