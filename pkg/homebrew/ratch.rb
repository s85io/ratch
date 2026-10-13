class Ratch < Formula
  desc "Detachable terminal sessions without terminal emulation"
  homepage "https://github.com/s85io/ratch"
  url "https://github.com/s85io/ratch/archive/refs/tags/v0.1.0.tar.gz"
  # sha256 "fill in from the release"
  license "GPL-2.0-or-later"
  head "https://github.com/s85io/ratch.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
    man1.install "ratch.1"
    bash_completion.install "completions/ratch.bash" => "ratch"
    zsh_completion.install "completions/_ratch"
    fish_completion.install "completions/ratch.fish"
  end

  test do
    assert_match "ratch #{version}", shell_output("#{bin}/ratch --version")
    system bin/"ratch", "start", "t", "sleep", "30"
    assert_match "t", shell_output("#{bin}/ratch list --names")
    system bin/"ratch", "kill", "-f", "t"
  end
end
