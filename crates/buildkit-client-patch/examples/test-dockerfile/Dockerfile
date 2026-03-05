# Simple test Dockerfile
FROM alpine:latest

# Add some labels
LABEL maintainer="test@example.com"
LABEL description="Test image for buildkit-client"

# Install some packages
RUN apk add --no-cache curl wget

# Create a test file
RUN echo "Hello from BuildKit!" > /hello.txt

# Set working directory
WORKDIR /app

# Add a simple script
RUN echo '#!/bin/sh' > /app/test.sh && \
    echo 'echo "BuildKit test successful!"' >> /app/test.sh && \
    chmod +x /app/test.sh

# Set entrypoint
ENTRYPOINT ["/app/test.sh"]
