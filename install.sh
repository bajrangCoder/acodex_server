#!/bin/bash

# detect architecture
detect_arch() {
    case $(uname -m) in
        armv7l | armv8l)
            echo "android-armv7"
            ;;
        aarch64)
            echo "android-arm64"
            ;;
        x86_64)
            echo "android-x86_64"
            ;;
        *)
            echo "Unsupported architecture. Please create an issue on GitHub, and we will consider providing a binary for your architecture."
            exit 1
            ;;
    esac
}

# download the appropriate binary
download_binary() {
    ARCH=$(detect_arch)
    BASE_URL="https://github.com/bajrangCoder/acodex_server/releases/latest/download"

    FILE_NAME="axs-$ARCH"
    DOWNLOAD_URL="$BASE_URL/$FILE_NAME"

    # Download the binary
    echo "Downloading $FILE_NAME for $ARCH architecture..."
    if ! curl --progress-bar --fail -L "$DOWNLOAD_URL" -o "$FILE_NAME"; then
        echo "Failed to download the binary! Please check the URL and your connection: $DOWNLOAD_URL"
        exit 1
    fi

    # Move the binary to the PREFIX directory and rename it to 'axs'
    echo "Installing axs binary to $PREFIX..."
    mv "$FILE_NAME" "$PREFIX/bin/axs"
    chmod +x "$PREFIX/bin/axs"

    echo "Binary downloaded and installed as 'axs'. You can now use the 'axs' command!"
    echo "Make sure '$PREFIX/bin' is in your PATH."
}

download_binary
