from __future__ import annotations

import signal
import subprocess
import sys
import time

from src.core import logger

log = logger.get('service')

_POLL_INTERVAL_SECONDS = 0.5
_SHUTDOWN_TIMEOUT_SECONDS = 10
_WORKER_NAME = 'worker'
_API_NAME = 'api'

_worker_cmd = [sys.executable, 'run.py']
_api_cmd = [sys.executable, '-m', 'src.api']

_shutdown_requested = False


def _signal_handler(signum: int, _frame: object) -> None:
    global _shutdown_requested
    _shutdown_requested = True
    try:
        signal_name = signal.Signals(signum).name
    except ValueError:
        signal_name = f'SIGNAL-{signum}'
    log.info('Received %s, stopping services', signal_name)


def _install_signal_handlers() -> None:
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, _signal_handler)


def _terminate_process(name: str, process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    log.info('Stopping %s (pid=%d)', name, process.pid)
    process.terminate()
    deadline = time.monotonic() + _SHUTDOWN_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        if process.poll() is not None:
            return
        time.sleep(0.2)
    if process.poll() is None:
        log.warning('%s did not stop in %ds, killing', name, _SHUTDOWN_TIMEOUT_SECONDS)
        process.kill()


def _spawn_process(command: list[str]) -> subprocess.Popen[bytes]:
    return subprocess.Popen(command)  # noqa: S603


def _run_all() -> int:
    processes = {
        _WORKER_NAME: _spawn_process(_worker_cmd),
        _API_NAME: _spawn_process(_api_cmd),
    }
    log.info(
        'Started worker(pid=%d) and api(pid=%d)',
        processes[_WORKER_NAME].pid,
        processes[_API_NAME].pid,
    )

    while True:
        if _shutdown_requested:
            for name, process in processes.items():
                _terminate_process(name, process)
            return 0

        for name, process in processes.items():
            return_code = process.poll()
            if return_code is None:
                continue
            if return_code == 0:
                log.warning('%s exited with code 0, shutting down remaining services', name)
            else:
                log.error('%s exited with code %d, shutting down remaining services', name, return_code)
            for other_name, other_process in processes.items():
                if other_name == name:
                    continue
                _terminate_process(other_name, other_process)
            return return_code

        time.sleep(_POLL_INTERVAL_SECONDS)


def main() -> None:
    log.info('Starting services (worker + api)')
    _install_signal_handlers()
    return_code = _run_all()
    if return_code != 0:
        raise SystemExit(return_code)
