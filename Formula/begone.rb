# A Homebrew formula for begone. It builds from source with Rust.
#
# Install the latest commit straight from the repo:
#   brew install --HEAD ./Formula/begone.rb
# For a tagged release, publish a tarball and fill in the `url` + `sha256`
# below (brew prints the sha256 it expects on the first try).
class Begone < Formula
  desc "Fast, honest, reversible Mac disk cleaner in your terminal"
  homepage "https://github.com/abhivirchaudhary/begone"
  license "MIT"
  head "https://github.com/abhivirchaudhary/begone.git", branch: "main"

  # For a tagged release, uncomment and fill these in:
  # url "https://github.com/abhivirchaudhary/begone/archive/refs/tags/v0.2.0.tar.gz"
  # sha256 "REPLACE_WITH_THE_TARBALL_SHA256"

  depends_on "rust" => :build
  depends_on :macos

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "reversible", shell_output("#{bin}/begone --help")
  end
end
