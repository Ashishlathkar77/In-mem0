# Garnet, fully broken down — why it leads at deep pipelining, and where we already match it

Third fact-checked research pass (103 agents) + our own profiling + clean two-machine benchmarks.
Date: 2026-06-22. This is the decisive synthesis.

## What Garnet actually does (verified)

- **It uses epoll, NOT io_uring.** Garnet runs on .NET's `SocketAsyncEventArgs` (epoll on Linux);
  io_uring in .NET is an experimental, opt-in, unshipped PR. So Garnet's ~15M ops/sec is achieved
  on **epoll** — the same primitive we can use.
- **Run-to-completion on the IO thread:** the IO-completion thread reads the socket, parses RESP,
  executes the storage op, and writes the response into a **pooled, sender-owned buffer**
  (`GarnetSaeaBuffer` from `LimitedFixedBufferPool`) — all inline, no hand-off, **zero per-op
  allocation**. Cache coherence brings data to the logic.
- **The headline numbers** (~10M@p16, ~15M@p64) come from Microsoft's **own `Resp.benchmark`** (not
  memtier), **8-byte** keys/values, batches up to **4096**, on **72-vCPU** Azure F72s v2, 80/20
  GET/SET. (Our memtier 16-vCPU 64-byte test is a different, harder config — but it was
  apples-to-apples between inmem and Garnet on the same box.)

## The key realization: the gap is per-op CPU, not the network model

We measured BOTH our network architectures on the clean two-machine rig:
- portable (thread-per-connection, blocking): **9.78M** @ P64
- io_uring/glommio (thread-per-core, run-to-completion): **9.66M** @ P64
- garnet (epoll, run-to-completion): **15.3M** @ P64

**Changing our network model didn't move the number** → the bottleneck is NOT the network
architecture. And the research confirms **io_uring barely helps for 64-byte messages** ("plain
epoll only marginally slower than io_uring"; zero-copy useless <1KiB; the famous 2.5× is only for
1MiB payloads). So there is **no single network technique** to copy — Garnet's lead is **per-op CPU
efficiency** in a heavily-tuned implementation (RESP parse + dispatch + pooled record management),
plus it was measured with 8-byte values (less memory traffic than our 64-byte test).

## Where we ALREADY match Garnet

On the clean two-machine rig at **P1 (no pipelining)**: inmem **1.04M** vs garnet **1.02M** — we
**tie/edge Garnet** in the latency-bound, non-pipelined regime, which is what the **majority of real
applications** actually do (few clients pipeline 64-deep). Research also notes Garnet's advantage is
strongest specifically in the deep-batch, high-session GET-heavy regime, and is beatable at low
thread counts. So:

- **inmem ties Garnet at p1 and beats Redis/Valkey/KeyDB/Dragonfly/Memcached everywhere.**
- **Garnet wins only the deep-pipelining throughput benchmark** — its purpose-built home turf.

## Honest verdict on "beat Garnet at deep pipelining"

It is **not a single fixable thing** — it's an open-ended per-op CPU grind (inline small values to
kill the per-SET malloc, SIMD SwissTable probing, a custom pooled record allocator, hand-tuned RESP
parse) against a mature Microsoft Research system, with **low confidence of fully closing 1.5×** and
diminishing returns. The profiling already removed the easy waste (encode/remove/alloc on writes).
The residual is hash lookup (~15%), RESP parse (~12%), and inherent recv/send (~23%).

## Concrete incremental levers (if pursuing, each small + uncertain)
1. **Inline small values** — store short string values inside the entry (no `Box` alloc per SET).
   Directly removes the per-SET malloc; biggest help with small values (Garnet tested 8-byte).
2. **SIMD SwissTable probing** in `FlatMap::get` (the 15% lookup).
3. **Faster small-key hash** (the benchmark keys are short).
4. A lean `mio` epoll thread-per-core loop — but our glommio run-to-completion already landed at
   portable's number, so expect little.

Each is a few-percent grind; stacking them *might* approach Garnet at deep pipelining, but there is
no guarantee, and Garnet remains excellent there.

## Recommendation
Declare the real, honest win — **inmem ties Garnet at p1 and beats every other cache** — and treat
deep-pipeline parity with Garnet as a long-tail performance project (the incremental levers above),
not a single achievable fix. We have, with data, fully mapped what separates the two.

Sources: Garnet docs (processing, network) + GitHub + MSR blog; dotnet/runtime #753 (io_uring
status); Garnet Resp.benchmark/results pages; io_uring/liburing (multishot, buf_ring, SQPOLL); the
io_uring-vs-epoll small-message measurements; arXiv 2510.19805 (independent scaling study).
