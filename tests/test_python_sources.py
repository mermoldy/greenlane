from pathlib import Path


def test_python_sources_compile() -> None:
    roots = [Path("src"), Path("tests")]
    files = [path for root in roots for path in root.glob("*.py")]

    assert files
    for path in files:
        compile(path.read_text(), str(path), "exec")
