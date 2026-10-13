# Copyright 2026 Gentoo Authors
# Distributed under the terms of the GNU General Public License v2

EAPI=8

inherit cargo git-r3 bash-completion-r1

DESCRIPTION="Detachable terminal sessions without terminal emulation"
HOMEPAGE="https://github.com/s85io/ratch"
EGIT_REPO_URI="https://github.com/s85io/ratch.git"

LICENSE="GPL-2+"
# Dependent crate licenses
LICENSE+=" Apache-2.0 Apache-2.0-with-LLVM-exceptions MIT"
SLOT="0"

src_unpack() {
	git-r3_src_unpack
	cargo_live_src_unpack
}

src_install() {
	cargo_src_install
	doman ratch.1
	dodoc README.md CHANGELOG.md
	newbashcomp completions/ratch.bash ratch
	insinto /usr/share/zsh/site-functions
	doins completions/_ratch
	insinto /usr/share/fish/vendor_completions.d
	doins completions/ratch.fish
}
