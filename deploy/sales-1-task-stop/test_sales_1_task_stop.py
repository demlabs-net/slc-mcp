"""Tests for sales-1-task-stop with fake SLC, Swarm and Docker."""
from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import subprocess
import sys
from pathlib import Path

import pytest

HERE = Path(__file__).parent
LOADER = importlib.machinery.SourceFileLoader("sales-1_task_stop", str(HERE / "sales-1-task-stop"))
SPEC = importlib.util.spec_from_loader("sales-1_task_stop", LOADER)
mod = importlib.util.module_from_spec(SPEC)
sys.modules["sales-1_task_stop"] = mod
LOADER.exec_module(mod)


class FakeClock:
    def __init__(self):
        self.t = 100_000.0

    def now(self):
        return self.t

    def sleep(self, seconds):
        self.t += seconds


class FakeDocker:
    def __init__(self, events, *, stop_works=True, comes_back=True):
        self.events = events
        self.up = {"hermes-crm-operator": True, "hermes-contactor": True}
        self.port_up = {8653: True, 8649: True}
        self.ports = {"hermes-crm-operator": 8653, "hermes-contactor": 8649}
        self.stop_works = stop_works
        self.comes_back = comes_back
        self.fail_inspect = False

    def running(self, container):
        if self.fail_inspect:
            raise subprocess.TimeoutExpired("docker inspect", 30)
        return self.up[container]

    def stop(self, container, timeout):
        self.events.append(("docker.stop", container))
        if self.stop_works:
            self.up[container] = False
            self.port_up[self.ports[container]] = False

    def start(self, container):
        self.events.append(("docker.start", container))
        if self.comes_back:
            self.up[container] = True
            self.port_up[self.ports[container]] = True

    def port_open(self, port):
        return self.port_up[port]

    def healthy(self, port):
        return self.port_up[port]


class FakeSlc:
    """Mirrors the SLC operator contract: claim fences, finalize releases."""

    def __init__(self, events):
        self.events = events
        self.requests = {
            "t1": {"task_id": "t1", "assignee": "crm-operator", "state": "requested",
                   "request_event_id": "ev-1", "claim_id": None},
        }
        self.task_status = {"t1": "IN_WORK"}
        self.fail_finalize = False
        self.fail_list = False
        self.next_head_woken_meanwhile = False

    def list_requests(self):
        if self.fail_list:
            raise mod.ControllerError("SLC down")
        return [dict(r) for r in self.requests.values()]

    def claim(self, task_id, claim_id):
        request = self.requests[task_id]
        if request["state"] == "claimed" and request["claim_id"] != claim_id:
            raise mod.ControllerError("claimed by someone else")
        if self.task_status[task_id] != "IN_WORK":
            request.update(state="abandoned", final_status=self.task_status[task_id])
            return dict(request)
        request.update(state="claimed", claim_id=claim_id)
        self.events.append(("slc.claim", task_id))
        return dict(request)

    def finalize(self, task_id, claim_id, evidence):
        if self.fail_finalize:
            raise mod.ControllerError("SLC down")
        request = self.requests[task_id]
        assert request["claim_id"] == claim_id
        assert evidence["container_running"] is False and evidence["api_port_open"] is False
        self.events.append(("slc.finalize", task_id))
        superseded = self.task_status[task_id] not in ("IN_WORK", "CANCELLED")
        if not superseded:
            self.task_status[task_id] = "CANCELLED"
        request["state"] = "abandoned" if superseded else "completed"
        return {
            "success": True,
            "superseded": superseded,
            "task": {"task_id": task_id, "status": self.task_status[task_id]},
            "next_ready_task": {"task_id": "t2"},
            "next_wake_recommended": True,
            "next_delivery": {"recipient": "crm-operator", "message": "SLC task t2 is ready",
                              "correlation_id": "t2", "idempotency_key": "queue-ready-t2"},
        }


class FakeSwarm:
    def __init__(self, events, slc):
        self.events = events
        self.slc = slc
        self.calls = []

    def call(self, name, arguments):
        self.calls.append((name, arguments))
        self.events.append(("swarm." + name, arguments.get("recipient")))
        return {"ok": True, "_is_error": False}

    def task_has_run(self, task_id, since):
        return self.slc.next_head_woken_meanwhile


def make(tmp_path, **docker_kwargs):
    clock = FakeClock()
    events = []
    config = mod.Config(slc_url="http://slc", slc_token="x" * 40, swarm_url="http://swarm",
                        swarm_token="y", manager_role="manager",
                        roles={"crm-operator": mod.RoleTarget("hermes-crm-operator", 8653),
                               "contactor": mod.RoleTarget("hermes-contactor", 8649)},
                        controller_id="test", settle_seconds=35)
    docker = FakeDocker(events, **docker_kwargs)
    slc = FakeSlc(events)
    swarm = FakeSwarm(events, slc)
    controller = mod.Controller(config, slc, swarm, docker, state_dir=tmp_path,
                                sleep=clock.sleep, now=clock.now, log=lambda m: None)
    return controller, slc, swarm, docker, events, clock


def test_full_stop_orders_claim_stop_settle_finalize_start_wake(tmp_path):
    controller, slc, swarm, docker, events, clock = make(tmp_path)
    started = clock.now()
    results = controller.run_once()
    assert results == [{"task_id": "t1", "status": "cancelled", "role": "crm-operator",
                        "next_task_id": "t2", "woke_next": True}]
    names = [event[0] for event in events]
    assert names == ["slc.claim", "docker.stop", "slc.finalize", "docker.start",
                     "slc.finalize", "swarm.dispatch_to"]
    assert clock.now() - started >= 35  # settle before releasing the lane
    assert swarm.calls[-1][1]["idempotency_key"] == "queue-ready-t2"
    assert not list(tmp_path.glob("pending-*.json"))


def test_task_finished_before_claim_leaves_executor_alone(tmp_path):
    controller, slc, swarm, docker, events, clock = make(tmp_path)
    slc.task_status["t1"] = "COMPLETED"
    results = controller.run_once()
    assert results[0]["status"] == "abandoned"
    assert not [e for e in events if e[0].startswith("docker.")]
    assert not swarm.calls


def test_report_that_landed_during_stop_is_kept_and_container_restarted(tmp_path):
    controller, slc, swarm, docker, events, clock = make(tmp_path)
    original_stop = docker.stop

    def stop_and_report(container, timeout):
        # The assignee's report was durable before the claim: SLC keeps it.
        slc.task_status["t1"] = "BLOCKED"
        original_stop(container, timeout)

    docker.stop = stop_and_report
    results = controller.run_once()
    assert results[0]["status"] == "superseded"
    assert docker.up["hermes-crm-operator"] is True  # never left stopped
    assert slc.task_status["t1"] == "BLOCKED"


def test_stop_that_does_not_take_effect_never_finalizes(tmp_path):
    controller, slc, swarm, docker, events, clock = make(tmp_path, stop_works=False)
    results = controller.run_once()
    assert results[0]["status"] == "held"
    assert ("slc.finalize", "t1") not in events
    assert slc.requests["t1"]["state"] == "claimed"  # lane stays reserved
    assert not swarm.calls


def test_crash_before_finalize_is_resumed_with_the_same_claim(tmp_path):
    controller, slc, swarm, docker, events, clock = make(tmp_path)
    slc.fail_finalize = True
    first = controller.run_once()
    assert first[0]["status"] == "held"
    marker = json.loads(next(tmp_path.glob("pending-*.json")).read_text())
    assert marker["stage"] == "stopping"
    assert docker.up["hermes-crm-operator"] is False  # still down: no run can start
    slc.fail_finalize = False
    second = controller.run_once()
    assert second[0]["status"] == "cancelled"
    assert slc.task_status["t1"] == "CANCELLED"
    assert docker.up["hermes-crm-operator"] is True
    assert not list(tmp_path.glob("pending-*.json"))


def test_container_that_does_not_come_back_keeps_marker_for_retry(tmp_path):
    controller, slc, swarm, docker, events, clock = make(tmp_path, comes_back=False)
    results = controller.run_once()
    assert results[0]["status"] == "held"
    assert slc.task_status["t1"] == "CANCELLED"  # already safely closed
    assert not swarm.calls  # nothing woken while the executor is down
    marker = json.loads(next(tmp_path.glob("pending-*.json")).read_text())
    assert marker["stage"] == "restarting"
    docker.comes_back = True
    again = controller.run_once()
    assert again[0]["status"] == "cancelled"
    assert swarm.calls and swarm.calls[-1][0] == "dispatch_to"


def test_wake_failure_never_causes_a_second_restart(tmp_path):
    controller, slc, swarm, docker, events, clock = make(tmp_path)

    def broken_call(name, arguments):
        raise mod.ControllerError("Swarm unreachable")

    swarm.call = broken_call
    results = controller.run_once()
    assert results[0]["status"] == "cancelled"
    assert results[0]["woke_next"] is False
    assert not list(tmp_path.glob("pending-*.json"))  # nothing left to "recover"
    events.clear()
    clock.sleep(400)
    assert controller.run_once() == []  # the next run does not touch the container
    assert not [e for e in events if e[0].startswith("docker.")]


def test_already_woken_next_head_is_not_woken_again(tmp_path):
    controller, slc, swarm, docker, events, clock = make(tmp_path)
    slc.next_head_woken_meanwhile = True
    results = controller.run_once()
    assert results[0]["woke_next"] is False
    assert not swarm.calls


def test_unexpected_error_holds_one_task_and_does_not_block_others(tmp_path):
    controller, slc, swarm, docker, events, clock = make(tmp_path)
    slc.requests["t0"] = {"task_id": "t0", "assignee": "contactor", "state": "requested",
                          "request_event_id": "ev-0", "claim_id": None}
    slc.task_status["t0"] = "IN_WORK"
    docker.fail_inspect = True
    results = controller.run_once()
    assert {r["status"] for r in results} == {"held"}
    assert all("TimeoutExpired" in r["error"] for r in results)
    docker.fail_inspect = False
    clock.sleep(400)  # past the per-container restart interval
    results = controller.run_once()
    assert sorted(r["status"] for r in results) == ["cancelled", "cancelled"]


def test_restart_rate_limit_waits_instead_of_restarting(tmp_path):
    controller, slc, swarm, docker, events, clock = make(tmp_path)
    controller.run_once()
    slc.requests["t3"] = {"task_id": "t3", "assignee": "crm-operator", "state": "requested",
                          "request_event_id": "ev-3", "claim_id": None}
    slc.task_status["t3"] = "IN_WORK"
    events.clear()
    results = controller.run_once()
    assert results[0]["status"] == "waiting"
    assert not events  # not even claimed: the executor keeps running meanwhile
    clock.sleep(400)
    results = controller.run_once()
    assert results[0]["status"] == "cancelled"


def test_listing_failure_still_reports_recoveries(tmp_path):
    controller, slc, swarm, docker, events, clock = make(tmp_path, comes_back=False)
    controller.run_once()  # leaves a restarting marker
    docker.comes_back = True
    slc.fail_list = True
    results = controller.run_once()
    assert results[0]["status"] == "cancelled"  # recovery ran before listing
    assert results[-1]["status"] == "held"


def test_unknown_role_holds(tmp_path):
    controller, slc, swarm, docker, events, clock = make(tmp_path)
    slc.requests["t1"]["assignee"] = "unknown-role"
    results = controller.run_once()
    assert results[0]["status"] == "held"
    assert not events


def test_parse_ts_handles_slc_and_swarm_formats():
    slc = mod.parse_ts("2026-09-29T17:07:56.123456789+00:00")
    swarm = mod.parse_ts("2026-09-29T17:08:24.500Z")
    assert slc is not None and swarm is not None and slc < swarm
    assert mod.parse_ts(None) is None and mod.parse_ts("garbage") is None


def test_task_has_run_ignores_dispatches_before_the_claim():
    swarm = mod.SwarmManager("http://swarm", "t")
    operations = {"operations": [
        # A wake from before the stop: the task was queued, so it never ran.
        {"kind": "dispatch_to", "status": "accepted", "created_at": "2026-09-29T10:00:00.000Z",
         "result": {"correlation_id": "t2", "run_id": "run_old"}, "deliveries": []},
    ]}
    swarm.read_resource = lambda uri: operations
    assert swarm.task_has_run("t2", "2026-09-29T17:07:56.1+00:00") is False
    operations["operations"].append(
        {"kind": "dispatch_to", "status": "accepted", "created_at": "2026-09-29T17:08:30.000Z",
         "result": {"correlation_id": "t2", "run_id": "run_new"}, "deliveries": []})
    assert swarm.task_has_run("t2", "2026-09-29T17:07:56.1+00:00") is True


def test_claim_id_is_host_independent(tmp_path):
    controller, *_ = make(tmp_path)
    assert controller.claim_id_for({"request_event_id": "ev-9"}) == "test:ev-9"


def test_config_parsing():
    config = mod.Config.from_env({
        "SLC_URL": "http://127.0.0.1:3000/",
        "SLC_OPERATOR_STOP_TOKEN": "t" * 40,
        "SWARM_MANAGER_MCP_URL": "http://127.0.0.1:3004/roles/manager/mcp",
        "MANAGER_SWARM_MCP_TOKEN": "m",
        "SALES_1_STOP_ROLES": "crm-operator=hermes-crm-operator:8653, contactor=hermes-contactor:8649",
    })
    assert config.slc_url == "http://127.0.0.1:3000"
    assert config.roles["contactor"].api_port == 8649
    assert config.controller_id == "sales-1-task-stop"
    with pytest.raises(mod.ControllerError):
        mod.Config.from_env({"SLC_URL": "x"})
