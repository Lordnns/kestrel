# Testing kestrel

Thanks for testing! This takes about two minutes. You only need the
`kestrel` program from this folder; the test programs are built into it.

## macOS (Apple Silicon or Intel)

1. Open **Terminal** (Spotlight: type "Terminal").
2. Go to the folder you unpacked. The easiest way: type `cd ` (with a space),
   drag the folder onto the Terminal window, press Enter.
3. macOS blocks programs downloaded from the internet that are not from the
   App Store. Clear that flag for this folder:

   ```
   xattr -dr com.apple.quarantine .
   ```

4. Run the self-test and save the result:

   ```
   ./kestrel selftest -v | tee report.txt
   ./kestrel info >> report.txt
   ```

5. Send back `report.txt`.

Alternative: double-click **Run self-test.command**. If macOS says it cannot
verify the developer, open System Settings → Privacy & Security, scroll down,
click **Open Anyway**, then double-click it again.

## Linux

```
./kestrel selftest -v | tee report.txt
./kestrel info >> report.txt
```

## Windows (PowerShell)

```
.\kestrel.exe selftest -v | Tee-Object report.txt
.\kestrel.exe info | Add-Content report.txt
```

## What a good result looks like

```
  PASS  hello  ...
  PASS  asm_checks  ...
  ...
12/12 passed  (... MIPS on the interp backend)
```

Any `FAIL` line, crash or error message is useful: please send it as is.

## Trying individual programs

The `examples` folder holds the same ARM64 programs as files:

```
./kestrel run examples/sha256.elf --stats
./kestrel run examples/asm_checks.elf --regs
```

## Building from source instead

If the downloaded binary will not run, build it yourself (needs internet once):

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# restart the terminal, then in the source folder:
cargo run --release -p kestrel-cli -- selftest -v
```
