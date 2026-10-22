"""A real install into the demo home, then `clank init` and a roster.

`cargo install` builds this checkout's committed HEAD into a demo-owned
root and target directory; the shell finds that binary first on PATH.
The user's own installation is never touched.
"""

from pathlib import Path

SCENES = ["install-init"]
TARGET_SECONDS = 70
SOURCE = Path(__file__).resolve().parents[3]


def run(ctx):
    ctx.sh(f"git clone -q {SOURCE} clank")
    shop = ctx.make_repo(
        "book-demo-shop",
        {"README.md": "# shop\n\nA tiny demo app.\n", "greet.sh": "#!/bin/sh\necho hello\n"},
    )
    real = ctx.real_home
    t = ctx.term(
        scene="install-init",
        env={
            "PATH": f"{ctx.demo}/.local/bin:{ctx.env['PATH']}",
            # The toolchain and crate cache are the user's; everything the
            # build writes goes under the demo home.
            "RUSTUP_HOME": f"{real}/.rustup",
            "CARGO_HOME": f"{real}/.cargo",
            "CARGO_TARGET_DIR": f"{ctx.demo}/target",
        },
    )
    t.expect(r"\$ $", 10)
    t.pause(0.8)
    t.type("cd ~/clank")
    t.type("cargo install --path crates/cli --locked --root ~/.local", settle=1)
    t.expect(r"Installed package", 60 * 60)
    t.expect(r"\$ $", 30)
    t.pause(1.5)
    t.type("which clank && clank --version")
    t.type(f"cd ~/{shop.name} && git log --oneline")
    t.type("clank init")
    t.expect(r"agent add", 10)
    t.pause(1.5)
    t.type("clank agent add alice --tool claude")
    t.expect(r"master", 10)
    t.type("clank agent add bob --tool codex")
    t.expect(r"reviewer", 10)
    t.pause(1)
    t.type("clank agent list")
    t.pause(2)
    t.type("git status --short")
    t.pause(3)
