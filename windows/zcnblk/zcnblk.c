// SPDX-License-Identifier: MIT
// Native Windows block edge. No networking, allocation/placement, RAID, or media.
#include <ntddk.h>
#include <storport.h>
#include <ntddscsi.h>
#include "zcnblk_ioctl.h"

#define ZC_SECTOR 4096u
#define ZC_MAX_PENDING 64u
#define ZC_LEASE_100NS UINT64_C(300000000)

typedef struct ZC_REQUEST {
    LIST_ENTRY Link;
    PSCSI_REQUEST_BLOCK Srb;
    ULONGLONG Id, Offset;
    ULONG Op, Length, Flags;
} ZC_REQUEST;

typedef struct ZC_ADAPTER {
    KSPIN_LOCK Lock;
    LIST_ENTRY Pending;
    ZC_REQUEST *Active;
    ULONG PendingCount;
    ULONGLONG NextId, Session, Capacity, LastHeartbeat;
    UCHAR UnitId[16];
    PFILE_OBJECT Owner;
    PVOID Timer;
    BOOLEAN Connected, Removing;
} ZC_ADAPTER;

C_ASSERT(FIELD_OFFSET(ZCNBLK_SERVICE_MESSAGE, Data) == ZCNBLK_SERVICE_HEADER);
C_ASSERT(sizeof(ZCNBLK_SERVICE_MESSAGE) == ZCNBLK_SERVICE_HEADER + ZCNBLK_SERVICE_MAX_IO);

static ULONGLONG ZcBe(const UCHAR *p, ULONG bytes)
{
    ULONGLONG n = 0;
    while (bytes--) n = (n << 8) | *p++;
    return n;
}

static VOID ZcPutBe(UCHAR *p, ULONGLONG n, ULONG bytes)
{
    while (bytes) { p[--bytes] = (UCHAR)n; n >>= 8; }
}

static VOID ZcError(PSCSI_REQUEST_BLOCK srb, UCHAR key, UCHAR asc)
{
    srb->SrbStatus = SRB_STATUS_ERROR;
    srb->ScsiStatus = SCSISTAT_CHECK_CONDITION;
    srb->DataTransferLength = 0;
    if (srb->SenseInfoBuffer && srb->SenseInfoBufferLength >= 18) {
        UCHAR *sense = srb->SenseInfoBuffer;
        RtlZeroMemory(sense, 18);
        sense[0] = 0x70; sense[2] = key; sense[7] = 10; sense[12] = asc;
        srb->SrbStatus |= SRB_STATUS_AUTOSENSE_VALID;
    }
}

static VOID ZcComplete(ZC_ADAPTER *a, PSCSI_REQUEST_BLOCK srb)
{
    StorPortNotification(RequestComplete, a, srb);
}

// Disconnect and retire all requests before releasing any SRB back to Storport.
static VOID ZcDetachChecked(ZC_ADAPTER *a, UCHAR status, BOOLEAN notify, ULONGLONG session, BOOLEAN expiredOnly)
{
    LIST_ENTRY failed;
    PFILE_OBJECT owner;
    KIRQL irql;
    InitializeListHead(&failed);
    KeAcquireSpinLock(&a->Lock, &irql);
    if ((session && (!a->Connected || a->Session != session)) ||
        (expiredOnly && KeQueryInterruptTime() - a->LastHeartbeat <= ZC_LEASE_100NS)) {
        KeReleaseSpinLock(&a->Lock, irql);
        return;
    }
    a->Connected = FALSE;
    owner = a->Owner; a->Owner = NULL;
    if (a->Active) { InsertTailList(&failed, &a->Active->Link); a->Active = NULL; }
    while (!IsListEmpty(&a->Pending)) {
        PLIST_ENTRY entry = RemoveHeadList(&a->Pending);
        InsertTailList(&failed, entry);
    }
    a->PendingCount = 0;
    KeReleaseSpinLock(&a->Lock, irql);
    while (!IsListEmpty(&failed)) {
        ZC_REQUEST *r = CONTAINING_RECORD(RemoveHeadList(&failed), ZC_REQUEST, Link);
        PSCSI_REQUEST_BLOCK srb = r->Srb;
        srb->SrbStatus = status; srb->DataTransferLength = 0;
        ZcComplete(a, srb);
    }
    if (owner) ObDereferenceObjectDeferDelete(owner);
    if (notify) StorPortNotification(BusChangeDetected, a, 0);
}

static VOID ZcDetach(ZC_ADAPTER *a, UCHAR status, BOOLEAN notify)
{
    ZcDetachChecked(a, status, notify, 0, FALSE);
}

static VOID ZcTimer(PVOID extension, PVOID context)
{
    ZC_ADAPTER *a = extension;
    KIRQL irql;
    BOOLEAN expired, removing;
    ULONGLONG session;
    UNREFERENCED_PARAMETER(context);
    KeAcquireSpinLock(&a->Lock, &irql);
    expired = a->Connected && KeQueryInterruptTime() - a->LastHeartbeat > ZC_LEASE_100NS;
    removing = a->Removing; session = a->Session;
    KeReleaseSpinLock(&a->Lock, irql);
    if (expired) ZcDetachChecked(a, SRB_STATUS_NO_DEVICE, TRUE, session, TRUE);
    KeAcquireSpinLock(&a->Lock, &irql);
    if (!removing && !a->Removing && a->Connected)
        StorPortRequestTimer(a, a->Timer, ZcTimer, NULL, 1000000, 0);
    KeReleaseSpinLock(&a->Lock, irql);
}

static BOOLEAN ZcInitialize(PVOID extension)
{
    ZC_ADAPTER *a = extension;
    if (STOR_STATUS_SUCCESS != StorPortInitializeTimer(a, &a->Timer)) return FALSE;
    return TRUE; // Arm the lease only when a userspace client connects.
}

static ULONG ZcFindAdapter(PVOID extension, PVOID context, PVOID bus,
    PVOID lower, PCHAR arguments, PPORT_CONFIGURATION_INFORMATION c, PBOOLEAN again)
{
    ZC_ADAPTER *a = extension;
    UNREFERENCED_PARAMETER(context); UNREFERENCED_PARAMETER(bus);
    UNREFERENCED_PARAMETER(lower); UNREFERENCED_PARAMETER(arguments);
    RtlZeroMemory(a, sizeof(*a));
    KeInitializeSpinLock(&a->Lock); InitializeListHead(&a->Pending); a->NextId = 1;
    c->MaximumTransferLength = ZCNBLK_SERVICE_MAX_IO;
    c->NumberOfPhysicalBreaks = 17;
    c->AlignmentMask = 0;
    c->NumberOfBuses = 1;
    c->MaximumNumberOfTargets = 1;
    c->MaximumNumberOfLogicalUnits = 1;
    c->ScatterGather = TRUE; c->Master = TRUE; c->CachesData = TRUE;
    c->WmiDataProvider = FALSE;
    c->SynchronizationModel = StorSynchronizeFullDuplex;
    c->VirtualDevice = TRUE;
    *again = FALSE;
    return SP_RETURN_FOUND;
}

static VOID ZcFreeAdapter(PVOID extension)
{
    ZC_ADAPTER *a = extension;
    KIRQL irql;
    KeAcquireSpinLock(&a->Lock, &irql); a->Removing = TRUE; KeReleaseSpinLock(&a->Lock, irql);
    if (a->Timer) {
        ULONG result;
        LARGE_INTEGER pause;
        pause.QuadPart = -10000;
        StorPortRequestTimer(a, a->Timer, ZcTimer, NULL, 0, 0);
        do {
            result = StorPortFreeTimer(a, a->Timer);
            if (result == STOR_STATUS_BUSY) KeDelayExecutionThread(KernelMode, FALSE, &pause);
        } while (result == STOR_STATUS_BUSY);
        a->Timer = NULL;
    }
    ZcDetach(a, SRB_STATUS_NO_DEVICE, FALSE);
}

static BOOLEAN ZcResetBus(PVOID extension, ULONG path)
{
    if (path) return FALSE;
    ZcDetach(extension, SRB_STATUS_BUS_RESET, TRUE);
    return TRUE;
}

static SCSI_ADAPTER_CONTROL_STATUS ZcAdapterControl(PVOID extension,
    SCSI_ADAPTER_CONTROL_TYPE type, PVOID parameters)
{
    if (type == ScsiQuerySupportedControlTypes) {
        PSCSI_SUPPORTED_CONTROL_TYPE_LIST list = parameters;
        if (ScsiStopAdapter < list->MaxControlType) list->SupportedTypeList[ScsiStopAdapter] = TRUE;
        if (ScsiRestartAdapter < list->MaxControlType) list->SupportedTypeList[ScsiRestartAdapter] = TRUE;
        return ScsiAdapterControlSuccess;
    }
    if (type == ScsiStopAdapter) {
        ZC_ADAPTER *a = extension;
        KIRQL irql;
        KeAcquireSpinLock(&a->Lock, &irql); a->Removing = TRUE; KeReleaseSpinLock(&a->Lock, irql);
        if (a->Timer) StorPortRequestTimer(a, a->Timer, ZcTimer, NULL, 0, 0);
        ZcDetach(a, SRB_STATUS_NO_DEVICE, FALSE);
        return ScsiAdapterControlSuccess;
    }
    if (type == ScsiRestartAdapter) {
        ZC_ADAPTER *a = extension;
        KIRQL irql;
        KeAcquireSpinLock(&a->Lock, &irql); a->Removing = FALSE; KeReleaseSpinLock(&a->Lock, irql);
        return ScsiAdapterControlSuccess;
    }
    return ScsiAdapterControlUnsuccessful;
}

static BOOLEAN ZcQueue(ZC_ADAPTER *a, PSCSI_REQUEST_BLOCK srb, ULONG op,
    ULONGLONG offset, ULONG len, ULONG flags)
{
    ZC_REQUEST *r = srb->SrbExtension;
    KIRQL irql;
    KeAcquireSpinLock(&a->Lock, &irql);
    if (!a->Connected || a->Removing) {
        KeReleaseSpinLock(&a->Lock, irql); srb->SrbStatus = SRB_STATUS_NO_DEVICE; return FALSE;
    }
    if (a->PendingCount >= ZC_MAX_PENDING) {
        KeReleaseSpinLock(&a->Lock, irql); srb->SrbStatus = SRB_STATUS_BUSY; return FALSE;
    }
    if (len > ZCNBLK_SERVICE_MAX_IO || offset > a->Capacity || len > a->Capacity - offset) {
        KeReleaseSpinLock(&a->Lock, irql); ZcError(srb, SCSI_SENSE_ILLEGAL_REQUEST, 0x21); return FALSE;
    }
    RtlZeroMemory(r, sizeof(*r));
    r->Srb = srb; r->Id = a->NextId++; r->Offset = offset; r->Length = len; r->Op = op; r->Flags = flags;
    InsertTailList(&a->Pending, &r->Link); a->PendingCount++;
    KeReleaseSpinLock(&a->Lock, irql);
    return TRUE;
}

static VOID ZcReply(PSCSI_REQUEST_BLOCK srb, const UCHAR *data, ULONG len, ULONG allocation)
{
    if (len > allocation) len = allocation;
    if (len > srb->DataTransferLength) len = srb->DataTransferLength;
    if (len && !srb->DataBuffer) { ZcError(srb, SCSI_SENSE_HARDWARE_ERROR, 0x44); return; }
    if (len) RtlCopyMemory(srb->DataBuffer, data, len);
    srb->DataTransferLength = len; srb->SrbStatus = SRB_STATUS_SUCCESS;
}

static BOOLEAN ZcStartIo(PVOID extension, PSCSI_REQUEST_BLOCK srb)
{
    ZC_ADAPTER *a = extension;
    UCHAR data[96], unit[16];
    ULONGLONG capacity;
    BOOLEAN connected;
    KIRQL irql;
    srb->ScsiStatus = SCSISTAT_GOOD;
    srb->SrbStatus = SRB_STATUS_SUCCESS;
    if (srb->Function == SRB_FUNCTION_PNP) { ZcComplete(a, srb); return TRUE; }
    KeAcquireSpinLock(&a->Lock, &irql);
    connected = a->Connected; capacity = a->Capacity; RtlCopyMemory(unit, a->UnitId, sizeof(unit));
    KeReleaseSpinLock(&a->Lock, irql);
    if (!connected || srb->PathId || srb->TargetId || srb->Lun) {
        srb->SrbStatus = SRB_STATUS_NO_DEVICE; srb->DataTransferLength = 0;
        ZcComplete(a, srb); return TRUE;
    }
    if (srb->Function == SRB_FUNCTION_FLUSH || srb->Function == SRB_FUNCTION_SHUTDOWN) {
        if (ZcQueue(a, srb, ZCNBLK_IO_SYNC, 0, 0, 0)) return TRUE;
        ZcComplete(a, srb); return TRUE;
    }
    if (srb->Function != SRB_FUNCTION_EXECUTE_SCSI) {
        srb->SrbStatus = SRB_STATUS_INVALID_REQUEST; ZcComplete(a, srb); return TRUE;
    }
    RtlZeroMemory(data, sizeof(data));
    switch (srb->Cdb[0]) {
    case 0x00: case 0x1b: case 0x1e: case 0x2f: case 0x8f:
        srb->DataTransferLength = 0; break;
    case 0x03:
        data[0] = 0x70; data[7] = 10; ZcReply(srb, data, 18, srb->Cdb[4]); break;
    case 0x12:
        if (srb->Cdb[1] & 1) {
            static const CHAR hex[] = "0123456789abcdef";
            ULONG i;
            data[1] = srb->Cdb[2];
            if (data[1] == 0) {
                data[3] = 3; data[4] = 0; data[5] = 0x80; data[6] = 0x83;
                ZcReply(srb, data, 7, srb->Cdb[4]);
            } else if (data[1] == 0x80 || data[1] == 0x83) {
                ULONG start = data[1] == 0x80 ? 4 : 16;
                if (data[1] == 0x83) { data[3] = 44; data[4] = 2; data[5] = 1; data[7] = 40; RtlCopyMemory(data + 8, "ZCUTILS ", 8); }
                else data[3] = 32;
                for (i = 0; i < 16; i++) { data[start + 2*i] = hex[unit[i] >> 4]; data[start + 2*i + 1] = hex[unit[i] & 15]; }
                ZcReply(srb, data, start + 32, srb->Cdb[4]);
            } else ZcError(srb, SCSI_SENSE_ILLEGAL_REQUEST, 0x24);
        } else {
            data[2] = 6; data[3] = 2; data[4] = 31; data[7] = 2;
            RtlFillMemory(data + 8, 28, ' ');
            RtlCopyMemory(data + 8, "ZCUTILS", 7);
            RtlCopyMemory(data + 16, "Native zcnblk", 13);
            RtlCopyMemory(data + 32, "0001", 4);
            ZcReply(srb, data, 36, srb->Cdb[4]);
        }
        break;
    case 0x25:
        ZcPutBe(data, (capacity / ZC_SECTOR - 1) > MAXULONG ? MAXULONG : capacity / ZC_SECTOR - 1, 4);
        ZcPutBe(data + 4, ZC_SECTOR, 4); ZcReply(srb, data, 8, 8); break;
    case 0x9e:
        if ((srb->Cdb[1] & 0x1f) != 0x10) { ZcError(srb, SCSI_SENSE_ILLEGAL_REQUEST, 0x24); break; }
        ZcPutBe(data, capacity / ZC_SECTOR - 1, 8); ZcPutBe(data + 8, ZC_SECTOR, 4);
        ZcReply(srb, data, 32, (ULONG)ZcBe(srb->Cdb + 10, 4)); break;
    case 0x1a: case 0x5a: {
        BOOLEAN ten = srb->Cdb[0] == 0x5a;
        ULONG head = ten ? 8 : 4;
        UCHAR page = srb->Cdb[2] & 0x3f;
        if (page != 0x08 && page != 0x3f) { ZcError(srb, SCSI_SENSE_ILLEGAL_REQUEST, 0x24); break; }
        if (ten) { data[1] = 26; data[3] = 0x10; } else { data[0] = 23; data[2] = 0x10; }
        data[head] = 0x08; data[head + 1] = 18; data[head + 2] = 0x04;
        ZcReply(srb, data, head + 20, ten ? (ULONG)ZcBe(srb->Cdb + 7, 2) : srb->Cdb[4]); break;
    }
    case 0xa0:
        data[3] = 8; ZcReply(srb, data, 16, (ULONG)ZcBe(srb->Cdb + 6, 4)); break;
    case 0x35: case 0x91:
        if (ZcQueue(a, srb, ZCNBLK_IO_SYNC, 0, 0, 0)) return TRUE;
        break;
    case 0x08: case 0x0a: case 0x28: case 0x2a: case 0x88: case 0x8a: {
        ULONGLONG lba, count;
        UCHAR op = srb->Cdb[0];
        BOOLEAN write = op == 0x0a || op == 0x2a || op == 0x8a;
        ULONG flags = (write && op != 0x0a && (srb->Cdb[1] & 8)) ? ZCNBLK_IO_FUA : 0;
        if (op == 0x08 || op == 0x0a) { lba = ZcBe(srb->Cdb + 1, 3) & 0x1fffff; count = srb->Cdb[4] ? srb->Cdb[4] : 256; }
        else if (op == 0x28 || op == 0x2a) { lba = ZcBe(srb->Cdb + 2, 4); count = ZcBe(srb->Cdb + 7, 2); }
        else { lba = ZcBe(srb->Cdb + 2, 8); count = ZcBe(srb->Cdb + 10, 4); }
        if (!count) { srb->DataTransferLength = 0; break; }
        if (count > ZCNBLK_SERVICE_MAX_IO / ZC_SECTOR || lba > capacity / ZC_SECTOR || count > capacity / ZC_SECTOR - lba || srb->DataTransferLength != count * ZC_SECTOR || !srb->DataBuffer) {
            ZcError(srb, SCSI_SENSE_ILLEGAL_REQUEST, 0x21); break;
        }
        if (ZcQueue(a, srb, write ? ZCNBLK_IO_WRITE : ZCNBLK_IO_READ, lba * ZC_SECTOR, (ULONG)count * ZC_SECTOR, flags)) return TRUE;
        break;
    }
    default: ZcError(srb, SCSI_SENSE_ILLEGAL_REQUEST, 0x20); break;
    }
    ZcComplete(a, srb);
    return TRUE;
}

static VOID ZcProcessService(PVOID extension, PVOID request)
{
    ZC_ADAPTER *a = extension;
    PIRP irp = request;
    PIO_STACK_LOCATION stack = IoGetCurrentIrpStackLocation(irp);
    // Storport may advance the service IRP stack without retaining FileObject
    // in that stack location. The original file object identifies its owner.
    PFILE_OBJECT file = irp->Tail.Overlay.OriginalFileObject;
    ZCNBLK_SERVICE_MESSAGE *m = irp->AssociatedIrp.SystemBuffer;
    ULONG input = stack->Parameters.DeviceIoControl.InputBufferLength;
    ULONG output = stack->Parameters.DeviceIoControl.OutputBufferLength;
    NTSTATUS status = STATUS_SUCCESS;
    ULONG information = ZCNBLK_SERVICE_HEADER;
    BOOLEAN changed = FALSE;
    PSCSI_REQUEST_BLOCK completed = NULL;
    KIRQL irql;
    irp->IoStatus.Information = 0;
    if (!m || input < ZCNBLK_SERVICE_HEADER || output < ZCNBLK_SERVICE_HEADER || m->Magic != ZCNBLK_SERVICE_MAGIC || m->Version != ZCNBLK_SERVICE_VERSION) { status = STATUS_INVALID_PARAMETER; goto done; }
    if (!file) file = stack->FileObject;
    // The I/O manager validates this IOCTL's FILE_READ_ACCESS|FILE_WRITE_ACCESS
    // against the opened Storport adapter handle before dispatch.
    if (!file) { status = STATUS_ACCESS_DENIED; goto done; }
    if (m->Command == ZCNBLK_SERVICE_DISCONNECT) {
        KeAcquireSpinLock(&a->Lock, &irql);
        changed = a->Connected && a->Owner == file && a->Session == m->Session;
        KeReleaseSpinLock(&a->Lock, irql);
        if (changed) ZcDetachChecked(a, SRB_STATUS_NO_DEVICE, TRUE, m->Session, FALSE);
        else status = STATUS_ACCESS_DENIED;
        goto done;
    }
    KeAcquireSpinLock(&a->Lock, &irql);
    if (a->Removing) { status = STATUS_DEVICE_REMOVED; goto unlock; }
    switch (m->Command) {
    case ZCNBLK_SERVICE_QUERY:
        m->Capacity = a->Capacity; m->Flags = a->Connected ? 1 : 0; m->Session = 0;
        break;
    case ZCNBLK_SERVICE_CONNECT:
        if (a->Connected) { status = STATUS_DEVICE_BUSY; break; }
        if (!m->Capacity || m->Capacity % ZC_SECTOR) { status = STATUS_INVALID_PARAMETER; break; }
        a->Capacity = m->Capacity; RtlCopyMemory(a->UnitId, m->UnitId, 16);
        a->Session = ++a->NextId; m->Session = a->Session;
        a->Owner = file; ObReferenceObject(a->Owner);
        a->LastHeartbeat = KeQueryInterruptTime(); a->Connected = TRUE;
        if (STOR_STATUS_SUCCESS != StorPortRequestTimer(a, a->Timer, ZcTimer, NULL, 1000000, 0)) {
            a->Connected = FALSE; a->Owner = NULL; ObDereferenceObjectDeferDelete(file);
            status = STATUS_INSUFFICIENT_RESOURCES;
        } else changed = TRUE;
        break;
    case ZCNBLK_SERVICE_EXCHANGE:
        if (!a->Connected || a->Owner != file || a->Session != m->Session) { status = STATUS_ACCESS_DENIED; break; }
        if (output < sizeof(*m)) { status = STATUS_BUFFER_TOO_SMALL; break; }
        a->LastHeartbeat = KeQueryInterruptTime();
        if (m->RequestId) {
            ZC_REQUEST *r = a->Active;
            if (!r || r->Id != m->RequestId || r->Op != m->Op || r->Length != m->Length || r->Offset != m->Offset) { status = STATUS_INVALID_PARAMETER; break; }
            if (!m->Status && r->Op == ZCNBLK_IO_READ && input < ZCNBLK_SERVICE_HEADER + r->Length) { status = STATUS_BUFFER_TOO_SMALL; break; }
            completed = r->Srb;
            if (m->Status) ZcError(completed, SCSI_SENSE_HARDWARE_ERROR, 0x44);
            else {
                if (r->Op == ZCNBLK_IO_READ) RtlCopyMemory(completed->DataBuffer, m->Data, r->Length);
                completed->SrbStatus = SRB_STATUS_SUCCESS; completed->ScsiStatus = SCSISTAT_GOOD; completed->DataTransferLength = r->Length;
            }
            a->Active = NULL;
        }
        m->RequestId = 0; m->Op = 0; m->Length = 0; m->Flags = 0; m->Status = 0;
        if (!a->Active && !IsListEmpty(&a->Pending)) {
            ZC_REQUEST *r = CONTAINING_RECORD(RemoveHeadList(&a->Pending), ZC_REQUEST, Link);
            a->PendingCount--; a->Active = r;
            m->RequestId = r->Id; m->Op = r->Op; m->Length = r->Length; m->Offset = r->Offset; m->Flags = r->Flags;
            if (r->Op == ZCNBLK_IO_WRITE) { RtlCopyMemory(m->Data, r->Srb->DataBuffer, r->Length); information += r->Length; }
        }
        break;
    default: status = STATUS_INVALID_DEVICE_REQUEST; break;
    }
unlock:
    KeReleaseSpinLock(&a->Lock, irql);
    if (completed) ZcComplete(a, completed);
    if (changed) StorPortNotification(BusChangeDetected, a, 0);
done:
    irp->IoStatus.Status = status;
    irp->IoStatus.Information = NT_SUCCESS(status) ? information : 0;
    StorPortCompleteServiceIrp(a, irp);
}

static VOID ZcCompleteService(PVOID extension)
{
    // Service IOCTLs complete synchronously; no caller IRPs are retained.
    UNREFERENCED_PARAMETER(extension);
}

// Keep a relocation entry even when Clang emits otherwise position-independent
// code: the Windows kernel loader requires a relocatable driver image.
static PHW_STARTIO volatile ZcStartIoAddress = ZcStartIo;

NTSTATUS DriverEntry(PDRIVER_OBJECT driver, PUNICODE_STRING registry)
{
    VIRTUAL_HW_INITIALIZATION_DATA init;
    RtlZeroMemory(&init, sizeof(init));
    init.HwInitializationDataSize = sizeof(init);
    init.AdapterInterfaceType = Internal;
    init.HwInitialize = ZcInitialize; init.HwFindAdapter = ZcFindAdapter;
    init.HwStartIo = ZcStartIoAddress; init.HwResetBus = ZcResetBus;
    init.HwAdapterControl = ZcAdapterControl; init.HwFreeAdapterResources = ZcFreeAdapter;
    init.HwProcessServiceRequest = ZcProcessService; init.HwCompleteServiceIrp = ZcCompleteService;
    init.DeviceExtensionSize = sizeof(ZC_ADAPTER); init.SrbExtensionSize = sizeof(ZC_REQUEST);
    init.MapBuffers = STOR_MAP_ALL_BUFFERS_INCLUDING_READ_WRITE;
    init.TaggedQueuing = TRUE; init.AutoRequestSense = TRUE; init.MultipleRequestPerLu = TRUE;
    return StorPortInitialize(driver, registry, (PHW_INITIALIZATION_DATA)&init, NULL);
}
