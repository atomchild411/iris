debug notes

Addresses below were read off a running machine with the IRIS monitor, not
taken from anywhere else. They are specific to the PROM image they were
observed with.

0xffffffff9fc31538 the 1000 instructions timing loop
ttyinput function
0xffffffff9fc3164c 27 bd ff d8     addiu      sp,sp,-0x28

consgetc 0xffffffff9fc20024
    actual read 0xffffffff9fc201e0
ttyinput 0xffffffff9fc3164c
ttypoll 0xffffffff9fc1fc84
circ_putc 0xffffffff9fc31888

## PROM environment variables for a verbose boot

Set from the PROM command monitor with `setenv`, and listed by `printenv`.
These names appear as strings in the PROM images themselves, so they can be
confirmed against whichever image is in use rather than taken on trust.

    setenv showconfig 1
    setenv diagmode v
    setenv kdebug 1

`showconfig` is the useful one: it makes device probing and initialisation
print what they are doing, which is what makes a device that fails to appear
in the hardware graph diagnosable at all. `diagmode` turns on more detailed
diagnostics in the standalone environment — note it is also what controls
whether the PROM continues past a failed power-on diagnostic, see
`docs/ip28-bringup.md`.

Some drivers carry their own flags, set the same way — `adp_verbose`,
`pcimh_verbose`, `plp_debug`. Which ones exist varies by platform; grep the
PROM image for a candidate before assuming it is honoured.

## CPU Execution Control

### `stop` Command Behavior

The `stop` command is used to halt CPU execution and works in two distinct modes:

1.  **Background Mode (Threaded)**
    *   **Initiated by:** `cpu start`
    *   **Execution:** The CPU runs in a dedicated background thread.
    *   **Stop Behavior:** The `stop` command sets the global `running` flag to `false` and synchronously waits (`join`) for the background thread to finish its current instruction block and terminate.
    *   **Result:** The command returns only after the CPU has fully stopped.

2.  **Debug Mode (Threaded)**
    *   **Initiated by:** `run`, `continue`, `step`, `next`, `finish`
    *   **Execution:** `run_debug_loop` runs in its own dedicated thread, equivalent to `start`. It does not require cloning the executor.
    *   **Async vs Sync:**
        *   `run`, `continue`: Call `run_debug_loop` asynchronously.
        *   `step`, `next`: Call `run_debug_loop` asynchronously but wait for it to finish (`join`).
    *   **Output:** `run_debug_loop` must not use `print!`. Output is either returned immediately before the thread starts or collected and returned to the monitor user when the thread finishes.
    *   **Stop Behavior:** The `stop` command sets the global `running` flag to `false`.
