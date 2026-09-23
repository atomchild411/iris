# Trace through devlog, not through a new environment variable

Written 2026-09-23, after finding that every IP28 bring-up tracer we added had
reinvented — worse — a facility that was already in the tree.

## The facility

`src/devlog.rs`. Twenty-six modules, each with a bitmask, each separately
redirectable, all controllable **while the machine runs**:

```
log status                 # what is on, at what mask, to what file
log mips mask cp0          # CP0 register traffic
log l2c  mask op           # secondary-cache tag operations
log l2c  file /tmp/ops.log # this module only, to a file
log all off
```

From startup instead: `[debug] debug_log = "l2c,mips"` in the config, or
`IRIS_DEBUG_LOG=l2c` in the environment.

The gate is `devlog_is_active(m) && devlog_mask(m) & BIT`: two relaxed atomic
loads. `mips_exec.rs` wraps it as `mips_log(bit)` (developer builds) and
`mips_log_always(bit)` (every build); `mips_cache_shadow.rs` as `l2_op_log`.

Categories: `mips` is `insn tlb mem fpu cp0`, `l1i`/`l1d`/`l2c` are
`hit miss op`.

## Why an `IRIS_SOMETHING=1` tracer is worse

Three reasons, and each of them cost us something real.

**It cannot be turned on late, or off early.** A bug that appears forty
seconds into a boot needs a tracer you can arm at second thirty-nine.

**It has nowhere to go but stderr.** The IP28 cache investigation hit this
head-on: lifting the trace cap produced 94k lines, which *"slowed the guest so
much the run never reached the loop being studied"*. A module file sink would
have made it a non-event — instead a principled filter had to be invented to
get the volume down. Measured after the port: 411,316 trace lines through a
file sink, POST still finishing in seven seconds.

**`std::env::var_os` is not free, and it is easy to leave uncached.**
`mips_cache_shadow.rs` called it **six times per `cache_op`**, with no
`OnceLock` anywhere in the file — a `getenv` on every CACHE instruction, in
every build, armed or not. `ip28_cp0_trace` did the same on every DMTC0, and
the XContext trace on every write to CP0 20.

## What stays an environment variable

A **parameter** — something the module mask cannot express:

- `IRIS_IP28_WATCHGPR=<n>` — which register to watch.
- `IRIS_IP28_TLBW=wired` — narrows the `tlb` category to entries below Wired.
- `IRIS_IP28_SS`, `IRIS_IP28_MCREV`, `IRIS_IP28_EXC_VADDR` — values, not
  traces.
- `IRIS_BREAK` — fault injection, not logging. See
  [`../irix/ip28-secondary-cache-contracts.md`](../irix/ip28-secondary-cache-contracts.md).

The rule: **devlog owns on/off and where it goes; an environment variable may
only carry a value devlog has no way to carry.**

## The trap that hid all of this

`IRIS_DEBUG_LOG=l2c ./iris --config foo.toml` produced *nothing*, and devlog
looked dead when only its bootstrap was. `DebugConfig::apply_env` called
`remove_var` whenever the config had no `debug_log` key — so the config file
deleted the variable the caller had just set, under a comment promising "env
vars still override if set externally". Fixed here, with tests.

Worth remembering in its general form: **when a facility looks unused, check
whether it is merely unreachable.** Ours had twenty-six modules and a monitor
command, and we wrote a dozen environment variables beside it.
