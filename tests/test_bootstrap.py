"""Materialize-and-compile tests for the injection template ``src/bootstrap.py``.

``bootstrap.py`` is not a standalone module: ``__TRACE_MODE__`` / ``__WARN_NS__`` /
``__SOCKET_PATH__`` are text-substituted and the ``# __GLR_ENCODER__`` marker is
replaced by the contents of ``src/glr.py`` by the Rust materializer (``fill_template``
in ``src/main.rs``) before injection. Importing the raw template fails, so we
replicate that substitution here and ``compile()`` the result — verifying the
template is materializable and the inlined encoder resolves ``GlrEnc`` at every
trace mode. This is the Python-side guard; the Rust suite (``src/trace_format.rs``)
runs the materialized bootstrap end-to-end, but only when ``python3``/``greenlet``
are present.
"""

from pathlib import Path

import pytest

_SRC = Path(__file__).resolve().parent.parent / "src"


def _fill_template(trace_mode, warn_ns=1_000_000, sock_path="/tmp/greenlane-test/control.sock"):
    """Port of ``fill_template`` (src/main.rs): inline glr.py at the encoder marker,
    then substitute the three runtime placeholders."""
    bootstrap = (_SRC / "bootstrap.py").read_text()
    encoder = (_SRC / "glr.py").read_text()
    return (
        bootstrap.replace("# __GLR_ENCODER__", encoder)
        .replace("__SOCKET_PATH__", sock_path)
        .replace("__TRACE_MODE__", str(trace_mode))
        .replace("__WARN_NS__", str(warn_ns))
    )


@pytest.mark.parametrize("trace_mode", [0, 1, 2])
def test_materialized_bootstrap_compiles(trace_mode):
    filled = _fill_template(trace_mode)
    # No placeholders should survive substitution.
    assert "__TRACE_MODE__" not in filled
    assert "__WARN_NS__" not in filled
    assert "__SOCKET_PATH__" not in filled
    assert "# __GLR_ENCODER__" not in filled
    # The inlined encoder must be present so names like GlrEnc resolve.
    assert "class GlrEnc" in filled
    # Compiles cleanly as an executable module (the form sys.remote_exec injects).
    compile(filled, "<greenlane-bootstrap>", "exec")


def test_raw_bootstrap_still_carries_its_placeholders():
    """Guard against the template being accidentally pre-filled in the repo."""
    raw = (_SRC / "bootstrap.py").read_text()
    assert "__TRACE_MODE__" in raw
    assert "__LONG_NS__" in raw
    assert "__SOCKET_PATH__" in raw
    assert "# __GLR_ENCODER__" in raw
