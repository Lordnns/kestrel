//! System call names, from the table on Switchbrew's "SVC" page, for
//! reports about calls kestrel does not implement. Generated from that
//! table.

/// The name of SVC `n`, if Switchbrew lists one.
pub fn svc_name(n: u16) -> Option<&'static str> {
    NAMES
        .binary_search_by_key(&n, |&(id, _)| id)
        .ok()
        .map(|i| NAMES[i].1)
}

const NAMES: &[(u16, &str)] = &[
    (0x01, "SetHeapSize"),
    (0x02, "SetMemoryPermission"),
    (0x03, "SetMemoryAttribute"),
    (0x04, "MapMemory"),
    (0x05, "UnmapMemory"),
    (0x06, "QueryMemory"),
    (0x07, "ExitProcess"),
    (0x08, "CreateThread"),
    (0x09, "StartThread"),
    (0x0a, "ExitThread"),
    (0x0b, "SleepThread"),
    (0x0c, "GetThreadPriority"),
    (0x0d, "SetThreadPriority"),
    (0x0e, "GetThreadCoreMask"),
    (0x0f, "SetThreadCoreMask"),
    (0x10, "GetCurrentProcessorNumber"),
    (0x11, "SignalEvent"),
    (0x12, "ClearEvent"),
    (0x13, "MapSharedMemory"),
    (0x14, "UnmapSharedMemory"),
    (0x15, "CreateTransferMemory"),
    (0x16, "CloseHandle"),
    (0x17, "ResetSignal"),
    (0x18, "WaitSynchronization"),
    (0x19, "CancelSynchronization"),
    (0x1a, "ArbitrateLock"),
    (0x1b, "ArbitrateUnlock"),
    (0x1c, "WaitProcessWideKeyAtomic"),
    (0x1d, "SignalProcessWideKey"),
    (0x1e, "GetSystemTick"),
    (0x1f, "ConnectToNamedPort"),
    (0x20, "SendSyncRequestLight"),
    (0x21, "SendSyncRequest"),
    (0x22, "SendSyncRequestWithUserBuffer"),
    (0x23, "SendAsyncRequestWithUserBuffer"),
    (0x24, "GetProcessId"),
    (0x25, "GetThreadId"),
    (0x26, "Break"),
    (0x27, "OutputDebugString"),
    (0x28, "ReturnFromException"),
    (0x29, "GetInfo"),
    (0x2a, "FlushEntireDataCache"),
    (0x2b, "FlushDataCache"),
    (0x2f, "GetLastThreadInfo"),
    (0x30, "GetResourceLimitLimitValue"),
    (0x31, "GetResourceLimitCurrentValue"),
    (0x32, "SetThreadActivity"),
    (0x40, "CreateSession"),
    (0x41, "AcceptSession"),
    (0x42, "ReplyAndReceiveLight"),
    (0x43, "ReplyAndReceive"),
    (0x44, "ReplyAndReceiveWithUserBuffer"),
    (0x45, "CreateEvent"),
    (0x4d, "SleepSystem"),
    (0x4e, "ReadWriteRegister"),
    (0x4f, "SetProcessActivity"),
    (0x50, "CreateSharedMemory"),
    (0x51, "MapTransferMemory"),
    (0x52, "UnmapTransferMemory"),
    (0x53, "CreateInterruptEvent"),
    (0x54, "QueryPhysicalAddress"),
    (0x56, "CreateDeviceAddressSpace"),
    (0x57, "AttachDeviceAddressSpace"),
    (0x58, "DetachDeviceAddressSpace"),
    (0x59, "MapDeviceAddressSpaceByForce"),
    (0x5a, "MapDeviceAddressSpaceAligned"),
    (0x5c, "UnmapDeviceAddressSpace"),
    (0x5d, "InvalidateProcessDataCache"),
    (0x5e, "StoreProcessDataCache"),
    (0x5f, "FlushProcessDataCache"),
    (0x60, "DebugActiveProcess"),
    (0x61, "BreakDebugProcess"),
    (0x62, "TerminateDebugProcess"),
    (0x63, "GetDebugEvent"),
    (0x64, "ContinueDebugEvent"),
    (0x65, "GetProcessList"),
    (0x66, "GetThreadList"),
    (0x67, "GetDebugThreadContext"),
    (0x68, "SetDebugThreadContext"),
    (0x69, "QueryDebugProcessMemory"),
    (0x6a, "ReadDebugProcessMemory"),
    (0x6b, "WriteDebugProcessMemory"),
    (0x6c, "SetHardwareBreakPoint"),
    (0x6d, "GetDebugThreadParam"),
    (0x70, "CreatePort"),
    (0x71, "ManageNamedPort"),
    (0x72, "ConnectToPort"),
    (0x73, "SetProcessMemoryPermission"),
    (0x74, "MapProcessMemory"),
    (0x75, "UnmapProcessMemory"),
    (0x76, "QueryProcessMemory"),
    (0x77, "MapProcessCodeMemory"),
    (0x78, "UnmapProcessCodeMemory"),
    (0x79, "CreateProcess"),
    (0x7a, "StartProcess"),
    (0x7b, "TerminateProcess"),
    (0x7c, "GetProcessInfo"),
    (0x7d, "CreateResourceLimit"),
    (0x7e, "SetResourceLimitLimitValue"),
    (0x7f, "CallSecureMonitor"),
];

#[cfg(test)]
mod tests {
    use super::svc_name;

    #[test]
    fn names_come_from_switchbrews_table() {
        assert_eq!(svc_name(0x01), Some("SetHeapSize"));
        assert_eq!(svc_name(0x26), Some("Break"));
        assert_eq!(svc_name(0x7f), Some("CallSecureMonitor"));
        assert_eq!(svc_name(0xff), None);
    }
}
