# Homebrew formula for AdaClaw
# To install: brew install worldflat21-lang/adaclaw/adaclaw
# Or tap first: brew tap worldflat21-lang/adaclaw
#               brew install adaclaw

class Adaclaw < Formula
  desc "Lightweight, secure, multi-channel Rust AI Agent Runtime"
  homepage "https://github.com/worldflat21-lang/AdaClaw"
  version "0.1.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/worldflat21-lang/AdaClaw/releases/download/v#{version}/adaclaw-macos-aarch64"
      sha256 "PLACEHOLDER_MACOS_AARCH64_SHA256"
    end
    on_intel do
      url "https://github.com/worldflat21-lang/AdaClaw/releases/download/v#{version}/adaclaw-macos-x86_64"
      sha256 "PLACEHOLDER_MACOS_X86_64_SHA256"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/worldflat21-lang/AdaClaw/releases/download/v#{version}/adaclaw-linux-aarch64"
      sha256 "PLACEHOLDER_LINUX_AARCH64_SHA256"
    end
    on_intel do
      url "https://github.com/worldflat21-lang/AdaClaw/releases/download/v#{version}/adaclaw-linux-x86_64"
      sha256 "PLACEHOLDER_LINUX_X86_64_SHA256"
    end
  end

  def install
    bin.install stable.url.split("/").last => "adaclaw"
    chmod 0755, bin/"adaclaw"
  end

  def post_install
    # Create default config directory
    (var/"adaclaw").mkpath
  end

  test do
    assert_match "adaclaw", shell_output("#{bin}/adaclaw --version")
  end
end
