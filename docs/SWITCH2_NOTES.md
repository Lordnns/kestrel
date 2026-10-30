# Switch 2 notes

Two kinds of entries live here (`PLAN.md`, section 2 and phase 2):

1. **Switch 2 differences from public sources.** Where the Switch 2 system
   interface differs from the documented Switch 1 interface, each with the
   public page it comes from.
2. **Behaviour descriptions from observation.** Anything learned by
   observing software rather than from public documentation is described
   here first and implemented from the description, never the other way
   round. That keeps the observation step and the implementation step
   separate, which is the core of the clean-room process.

Never record anything taken from leaked SDK material, NVN headers or
documentation, private forks, or decompiled console code (`PLAN.md`,
section 2). If the only source for a fact is one of those, leave it out.

## Entry format

```
### <Short title>

- Kind: public source | observation
- Source: <page and section> | <what was observed, how, on what software
  and hardware, by whom, and when>
- Behaviour: <what happens, in our own words>
- Used by: <kestrel module or task, once implemented>
```

An observation names the software (homebrew built from source, or a title
the owner legally has) but never includes its code or data.

## Public sources

### Svc 0x80, SetGpuMemoryAttribute

- Kind: public source
- Source: Switchbrew, "SVC", system call table (entry marked [S2]).
- Behaviour: a Switch 2 system call taking an address, a size, a mask and
  an attribute value, like svcSetMemoryAttribute. Its semantics are not
  documented there.
- Used by: not implemented; an unknown call stops with its number.

### MemoryAttribute bits 5 and 6

- Kind: public source
- Source: Switchbrew, "SVC", MemoryAttribute table (bits marked [S2]).
- Behaviour: bit 5 is GpuSharable and bit 6 GpuShared on Switch 2. Their
  meaning is not described there.
- Used by: not implemented; kestrel never sets them.

### Errors from the SendSyncRequest calls close the session

- Kind: public source
- Source: Switchbrew, "SVC", svcSendSyncRequest (note marked [S2]).
- Behaviour: on Switch 2, every error the SendSyncRequest calls return
  closes the session. The page does not know whether the kernel or the
  system module does it.
- Used by: not implemented; kestrel's errors keep the session open, and
  requests it cannot answer stop the process.

### set:sys GetFirmwareVersion removed

- Kind: public source
- Source: Switchbrew, "Settings services", set:sys GetFirmwareVersion
  (command 3): "This was removed with S2".
- Behaviour: on Switch 2, set:sys no longer has command 3;
  GetFirmwareVersion2 (command 4) returns the same structure.
- Used by: `hle/service/set.rs` answers command 4 and stops on command 3
  with the note.

## Observations

None yet.
