# roy1-task-stop — operator stop controller for running tasks

SLC does not own the executor process, so `cancel_task` on a RUNNING task does
not release the lane by itself: it records a durable stop request
(`stop_requested: true`). This host-side controller executes those requests on
a swarm host where every role is a Hermes docker container:

1. claims the request in SLC (from then on the old run can neither
   `report_task` nor `start_task`);
2. stops the role's container and verifies that it is down (container not
   running, API port closed);
3. waits `ROY1_SETTLE_SECONDS` so requests the killed run already sent can land;
4. finalizes the stop in SLC: the task becomes CANCELLED (or keeps the
   assignee's own report if it finished first), the lane is released and the
   next FIFO head is promoted;
5. starts the container again and waits for its `/health`;
6. wakes the new READY head through Swarm `dispatch_to` (with the SLC ready
   event id as the idempotency key) unless it already has a run.

Any failure before step 4 leaves the lane reserved. A marker written before the
container is stopped lets the next run finish an interrupted stop (start the
container, wake the head). Stopping a container ends every run of that role,
so restarts are rate limited per container.

## Install

```sh
install -m 0755 roy1-task-stop /usr/local/bin/roy1-task-stop
install -m 0644 roy1-task-stop.service roy1-task-stop.timer /usr/lib/systemd/system/
install -d -m 0700 /etc/roy1-task-stop
install -m 0600 env.example /etc/roy1-task-stop/env   # then fill it in
systemctl daemon-reload && systemctl enable --now roy1-task-stop.timer
```

SLC needs, in its own environment:

- `SLC_OPERATOR_STOP_TOKEN` — at least 32 characters, the same value as in
  `/etc/roy1-task-stop/env`. Without it the operator endpoints answer 503.
- `SLC_STOPPABLE_PRINCIPALS` — comma-separated roles this controller serves
  (the role list of `ROY1_STOP_ROLES`). `cancel_task` refuses to stop a running
  task of any other role, because nobody would execute the request.

Only the global task coordinator (a principal granted `"*"` in
`SLC_TASK_ASSIGN_ACL`, e.g. `{"manager":["*"]}`) may stop a running task.

## Operate

- `roy1-task-stop --list` prints the current stop requests.
- `journalctl -u roy1-task-stop` shows each step; the task's own event stream
  (`list_task_events`) shows the stop milestones written by `operator-stop`.
- State (restart times, recovery markers) lives in `/var/lib/roy1-task-stop`.

## Test

```sh
python3 -m pytest deploy/roy1-task-stop/test_roy1_task_stop.py
```
