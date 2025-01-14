#!/bin/bash

# detect architecture
detect_arch() {
    case $(uname -m) in
        armv7l)
            echo "android-armv7"
            ;;
        aarch64 | armv8l)
            echo "android-arm64"
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
        echo "Failed to download the binary!"
        exit 1
    fi

    # Move the binary to the PREFIX directory and rename it to 'axs'
    echo "Installing axs binary to $PREFIX..."
    mv "$FILE_NAME" "$PREFIX/bin/axs"
    chmod +x "$PREFIX/bin/axs"

    echo "Binary downloaded and installed as 'axs'. You can now use the 'axs' command!"
}

download_binary
