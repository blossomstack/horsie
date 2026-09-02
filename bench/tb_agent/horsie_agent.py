"""A Terminal-Bench agent that drives horsie.

Every other Terminal-Bench agent installs a CLI in the task container and types
at it. horsie is inverted: the *agent* runs on a horsie server, and only a thin
runtime lives in the container, dialling out to reach it. So this adapter does
two things at once --

  in the container:  install the horsie CLI and run `horsie connect`, which
                     publishes the container as a runtime vendor
  on the host:       create a session against that vendor and drive it over the
                     HTTP API, since the adapter already has network

-- and the tmux session is used only for setup. There is nothing to type at.

Consequences worth knowing:

* The container needs to reach the horsie server, and nothing else. That is one
  outbound websocket; a task environment with no general internet is fine as
  long as that one host is routable.
* The asciinema recording will be near-empty, because the agent's work happens
  through horsie's own runtime rather than through the pane. Scoring is
  unaffected -- Terminal-Bench grades by running the task's tests.

Install by pointing Terminal-Bench at this class, and configure with
`--agent-kwarg`:

    uv run tb run --task-id hello-world \\
        --agent-import-path bench.tb_agent.horsie_agent:HorsieAgent \\
        --agent-kwarg model_name=gpt-5.6-luna

Secrets come from the environment (`HORSIE_URL`, `HORSIE_TOKEN`), never from
kwargs, so they stay out of Terminal-Bench's run manifests.
"""

from __future__ import annotations

import os
import shlex
import sys
import time
import uuid
from pathlib import Path

from terminal_bench.agents.base_agent import AgentResult, BaseAgent
from terminal_bench.agents.failure_mode import FailureMode
from terminal_bench.terminal.tmux_session import TmuxSession

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from horsie_client import Horsie, HorsieError  # noqa: E402
from horsie_turn import usage_of, wait_for_turn  # noqa: E402

_INSTALL_SCRIPT = Path(__file__).parent / "install-horsie.sh"
_CONTAINER_DIR = "/horsie-agent"
_CONNECT_LOG = f"{_CONTAINER_DIR}/connect.log"


class HorsieAgent(BaseAgent):
    def __init__(
        self,
        model_name: str | None = None,
        workdir: str = "/app",
        binaries_dir: str | None = None,
        timeout_sec: float = 1200.0,
        connect_timeout_sec: float = 90.0,
        effort: str | None = None,
        max_iterations: int | None = None,
        **kwargs,
    ) -> None:
        super().__init__(**kwargs)
        self._model = model_name or os.environ.get("HORSIE_MODEL") or ""
        self._workdir = workdir
        # Statically linked horsie/horsie-runtime to copy in. Strongly preferred
        # over the network installer: the published Linux builds need glibc
        # >= 2.38 and many task images are older, so the download path fails on
        # exactly the images a benchmark cares about.
        self._binaries_dir = binaries_dir or os.environ.get("HORSIE_BIN_DIR") or None
        self._timeout_sec = float(timeout_sec)
        self._connect_timeout_sec = float(connect_timeout_sec)
        self._effort = effort
        self._max_iterations = int(max_iterations) if max_iterations else None

        self._server = os.environ.get("HORSIE_URL", "").rstrip("/")
        self._token = os.environ.get("HORSIE_TOKEN", "")
        self._project = os.environ.get("HORSIE_PROJECT", "1")
        if not self._model:
            raise ValueError("set --agent-kwarg model_name=<alias> or HORSIE_MODEL")
        if not self._server or not self._token:
            raise ValueError("HORSIE_URL and HORSIE_TOKEN must be set in the environment")

    @staticmethod
    def name() -> str:
        return "horsie"

    # ------------------------------------------------------------- container

    def _exec(self, session: TmuxSession, script: str, env: dict[str, str] | None = None):
        """Run a shell snippet in the container and return (exit_code, output).

        Uses the docker exec API rather than the tmux pane. Setup driven through
        a terminal has to survive quoting, prompt detection and buffer capture;
        none of that is load-bearing here, and all of it fails obscurely.
        """
        result = session.container.exec_run(
            ["sh", "-c", script],
            environment=env or {},
        )
        output = result.output
        if isinstance(output, bytes):
            output = output.decode(errors="replace")
        return result.exit_code, output or ""

    def _install(self, session: TmuxSession) -> tuple[bool, str]:
        if self._binaries_dir:
            src = Path(self._binaries_dir)
            missing = [n for n in ("horsie", "horsie-runtime") if not (src / n).is_file()]
            if missing:
                return False, f"binaries_dir {src} is missing: {', '.join(missing)}"
            # Both, not just the runtime: `horsie connect` spawns horsie-runtime
            # as a child and finds it beside its own executable.
            session.copy_to_container(
                [src / "horsie", src / "horsie-runtime"],
                container_dir=f"{_CONTAINER_DIR}/bin",
            )
            self._exec(session, f"chmod +x {_CONTAINER_DIR}/bin/horsie*")

        session.copy_to_container(
            _INSTALL_SCRIPT,
            container_dir=_CONTAINER_DIR,
            container_filename="install-horsie.sh",
        )
        code, out = self._exec(
            session,
            f"sh {_CONTAINER_DIR}/install-horsie.sh 2>&1",
            env={"HORSIE_URL": self._server, "HORSIE_TOKEN": self._token},
        )
        return ("HORSIE_INSTALL_OK" in out and code == 0), out

    def _start_connect(self, session: TmuxSession, vendor: str) -> tuple[bool, str]:
        """Publish this container as a runtime vendor and wait until it is live.

        `--no-sandbox` because the container is already the isolation boundary;
        a second sandbox inside it only blocks the runtime's own writes.

        Readiness is read from the connect log rather than assumed after a
        sleep: the dial-out has to traverse the network to the server, and a
        session created against a vendor that has not announced itself fails in
        a way that looks like a model problem.
        """
        cmd = (
            f"mkdir -p {_CONTAINER_DIR} && "
            f"PATH={_CONTAINER_DIR}/bin:/root/.local/bin:$PATH nohup horsie connect "
            f"--server {shlex.quote(self._server)} "
            f"--workspace {shlex.quote(self._workdir)} "
            f"--name {shlex.quote(vendor)} "
            f"--no-sandbox > {_CONNECT_LOG} 2>&1 &"
        )
        self._exec(session, cmd)

        deadline = time.monotonic() + self._connect_timeout_sec
        log = ""
        while time.monotonic() < deadline:
            _, log = self._exec(session, f"cat {_CONNECT_LOG} 2>/dev/null || true")
            if "connected to" in log:
                return True, log
            # The CLI reports an unusable server or a rejected token and exits;
            # waiting out the full timeout on a dead process helps nobody.
            if "error" in log.lower() or "failed" in log.lower():
                return False, log
            time.sleep(2.0)
        return False, log

    # ----------------------------------------------------------------- entry

    def perform_task(
        self,
        instruction: str,
        session: TmuxSession,
        logging_dir: Path | None = None,
    ) -> AgentResult:
        ok, out = self._install(session)
        if not ok:
            self._log(logging_dir, "install.log", out)
            return AgentResult(failure_mode=FailureMode.AGENT_INSTALLATION_FAILED)

        vendor = f"tb-{uuid.uuid4().hex[:12]}"
        ok, log = self._start_connect(session, vendor)
        if not ok:
            self._log(logging_dir, "connect.log", log)
            return AgentResult(failure_mode=FailureMode.AGENT_INSTALLATION_FAILED)

        h = Horsie(self._server, self._token, self._project)
        session_id = ""
        try:
            session_id = h.create_session(
                message=self._render_instruction(instruction),
                model=self._model,
                vendor=vendor,
                name=f"tb {vendor}",
                max_iterations=self._max_iterations,
                thinking_effort=self._effort,
            )
            status, detail = wait_for_turn(
                h, session_id, timeout_s=self._timeout_sec, poll_s=5.0
            )
        except HorsieError as e:
            self._log(logging_dir, "horsie-error.log", str(e))
            return AgentResult(failure_mode=FailureMode.UNKNOWN_AGENT_ERROR)

        usage = usage_of(detail)
        self._log(
            logging_dir,
            "horsie-session.log",
            f"session={session_id}\nvendor={vendor}\nstatus={status}\nusage={usage}\n"
            f"lastError={detail.get('lastError')}\n",
        )

        if status == "Timeout":
            failure = FailureMode.AGENT_TIMEOUT
        elif status in ("Failed", "Unrecoverable"):
            failure = FailureMode.UNKNOWN_AGENT_ERROR
        else:
            failure = FailureMode.NONE

        # Token counts are reported even on failure: the tokens were spent
        # whatever became of the turn, and a run whose cost silently vanishes on
        # the failures is a run whose cost is understated exactly where it is
        # most interesting.
        return AgentResult(
            total_input_tokens=usage["input_tokens"],
            total_output_tokens=usage["output_tokens"],
            failure_mode=failure,
        )

    @staticmethod
    def _log(logging_dir: Path | None, name: str, content: str) -> None:
        if logging_dir is None:
            return
        try:
            logging_dir.mkdir(parents=True, exist_ok=True)
            (logging_dir / name).write_text(content)
        except OSError:
            pass
