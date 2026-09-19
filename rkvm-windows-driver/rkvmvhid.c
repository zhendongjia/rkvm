#include <ntddk.h>
#include <wdf.h>
#include <wdmsec.h>
#include <vhf.h>

#define RKVM_KEYBOARD_REPORT_ID 1
#define RKVM_MOUSE_REPORT_ID 2
#define RKVM_CONSUMER_REPORT_ID 3
#define RKVM_SYSTEM_REPORT_ID 4
#define RKVM_KEYBOARD_REPORT_SIZE 9
#define RKVM_MOUSE_REPORT_SIZE 6
#define RKVM_CONSUMER_REPORT_SIZE 2
#define RKVM_SYSTEM_REPORT_SIZE 2

DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD RkvmEvtDeviceAdd;
EVT_WDF_OBJECT_CONTEXT_CLEANUP RkvmEvtDeviceCleanup;
EVT_WDF_FILE_CLEANUP RkvmEvtFileCleanup;
EVT_WDF_IO_QUEUE_IO_WRITE RkvmEvtIoWrite;

#ifdef ALLOC_PRAGMA
#pragma alloc_text(INIT, DriverEntry)
#pragma alloc_text(PAGE, RkvmEvtDeviceAdd)
#pragma alloc_text(PAGE, RkvmEvtDeviceCleanup)
#pragma alloc_text(PAGE, RkvmEvtFileCleanup)
#endif

typedef struct _RKVM_DEVICE_CONTEXT {
    VHFHANDLE VhfHandle;
} RKVM_DEVICE_CONTEXT, *PRKVM_DEVICE_CONTEXT;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(RKVM_DEVICE_CONTEXT, RkvmGetDeviceContext)

static UCHAR RkvmReportDescriptor[] = {
    // Keyboard top-level collection, report ID 1. The payload is the standard
    // boot-keyboard layout: modifiers, reserved byte, and six HID usages.
    0x05, 0x01,       // Usage Page (Generic Desktop)
    0x09, 0x06,       // Usage (Keyboard)
    0xA1, 0x01,       // Collection (Application)
    0x85, RKVM_KEYBOARD_REPORT_ID,
    0x05, 0x07,       // Usage Page (Keyboard/Keypad)
    0x19, 0xE0,       // Usage Minimum (Left Control)
    0x29, 0xE7,       // Usage Maximum (Right GUI)
    0x15, 0x00,
    0x25, 0x01,
    0x75, 0x01,
    0x95, 0x08,
    0x81, 0x02,       // Input (Data, Variable, Absolute)
    0x95, 0x01,
    0x75, 0x08,
    0x81, 0x03,       // Input (Constant)
    0x05, 0x08,       // Usage Page (LEDs)
    0x19, 0x01,
    0x29, 0x05,
    0x95, 0x05,
    0x75, 0x01,
    0x91, 0x02,       // Output (Data, Variable, Absolute)
    0x95, 0x01,
    0x75, 0x03,
    0x91, 0x03,       // Output (Constant)
    0x05, 0x07,
    0x19, 0x00,
    0x29, 0x65,
    0x15, 0x00,
    0x25, 0x65,
    0x95, 0x06,
    0x75, 0x08,
    0x81, 0x00,       // Input (Data, Array, Absolute)
    0xC0,

    // Five-button relative mouse, report ID 2. X, Y, vertical wheel and
    // horizontal wheel are signed 8-bit deltas.
    0x05, 0x01,       // Usage Page (Generic Desktop)
    0x09, 0x02,       // Usage (Mouse)
    0xA1, 0x01,       // Collection (Application)
    0x85, RKVM_MOUSE_REPORT_ID,
    0x09, 0x01,       // Usage (Pointer)
    0xA1, 0x00,       // Collection (Physical)
    0x05, 0x09,       // Usage Page (Button)
    0x19, 0x01,
    0x29, 0x05,
    0x15, 0x00,
    0x25, 0x01,
    0x95, 0x05,
    0x75, 0x01,
    0x81, 0x02,
    0x95, 0x01,
    0x75, 0x03,
    0x81, 0x03,
    0x05, 0x01,
    0x09, 0x30,       // Usage (X)
    0x09, 0x31,       // Usage (Y)
    0x09, 0x38,       // Usage (Wheel)
    0x15, 0x81,       // Logical Minimum (-127)
    0x25, 0x7F,       // Logical Maximum (127)
    0x75, 0x08,
    0x95, 0x03,
    0x81, 0x06,       // Input (Data, Variable, Relative)
    0x05, 0x0C,       // Usage Page (Consumer)
    0x0A, 0x38, 0x02, // Usage (AC Pan)
    0x15, 0x81,
    0x25, 0x7F,
    0x75, 0x08,
    0x95, 0x01,
    0x81, 0x06,
    0xC0,
    0xC0,

    // Audio controls, report ID 3. Independent bits preserve simultaneous
    // presses: mute, volume decrement, volume increment, then five pad bits.
    0x05, 0x0C,       // Usage Page (Consumer)
    0x09, 0x01,       // Usage (Consumer Control)
    0xA1, 0x01,       // Collection (Application)
    0x85, RKVM_CONSUMER_REPORT_ID,
    0x09, 0xE2,       // Usage (Mute)
    0x09, 0xEA,       // Usage (Volume Decrement)
    0x09, 0xE9,       // Usage (Volume Increment)
    0x15, 0x00,
    0x25, 0x01,
    0x75, 0x01,
    0x95, 0x03,
    0x81, 0x02,       // Input (Data, Variable, Absolute)
    0x75, 0x05,
    0x95, 0x01,
    0x81, 0x03,       // Input (Constant)
    0xC0,

    // Standard power button, report ID 4. Windows applies the user's power
    // button policy; the client does not issue shutdown or suspend commands.
    0x05, 0x01,       // Usage Page (Generic Desktop)
    0x09, 0x80,       // Usage (System Control)
    0xA1, 0x01,       // Collection (Application)
    0x85, RKVM_SYSTEM_REPORT_ID,
    0x09, 0x81,       // Usage (System Power Down)
    0x15, 0x00,
    0x25, 0x01,
    0x75, 0x01,
    0x95, 0x01,
    0x81, 0x02,       // Input (Data, Variable, Absolute)
    0x75, 0x07,
    0x95, 0x01,
    0x81, 0x03,       // Input (Constant)
    0xC0,
};

static BOOLEAN RkvmValidReport(_In_reads_bytes_(Length) const UCHAR *Report, size_t Length)
{
    if (Length == RKVM_KEYBOARD_REPORT_SIZE && Report[0] == RKVM_KEYBOARD_REPORT_ID) {
        return TRUE;
    }
    if (Length == RKVM_MOUSE_REPORT_SIZE && Report[0] == RKVM_MOUSE_REPORT_ID) {
        return TRUE;
    }
    if (Length == RKVM_CONSUMER_REPORT_SIZE && Report[0] == RKVM_CONSUMER_REPORT_ID) {
        return TRUE;
    }
    if (Length == RKVM_SYSTEM_REPORT_SIZE && Report[0] == RKVM_SYSTEM_REPORT_ID) {
        return TRUE;
    }
    return FALSE;
}

static NTSTATUS RkvmSubmitReport(
    _In_ PRKVM_DEVICE_CONTEXT Context,
    _In_reads_bytes_(Length) UCHAR *Report,
    size_t Length)
{
    HID_XFER_PACKET packet;

    if (Context->VhfHandle == NULL) {
        return STATUS_DEVICE_NOT_READY;
    }
    packet.reportBuffer = Report;
    packet.reportBufferLen = (ULONG)Length;
    packet.reportId = Report[0];
    return VhfReadReportSubmit(Context->VhfHandle, &packet);
}

NTSTATUS DriverEntry(_In_ PDRIVER_OBJECT DriverObject, _In_ PUNICODE_STRING RegistryPath)
{
    WDF_DRIVER_CONFIG config;

    WDF_DRIVER_CONFIG_INIT(&config, RkvmEvtDeviceAdd);
    return WdfDriverCreate(
        DriverObject,
        RegistryPath,
        WDF_NO_OBJECT_ATTRIBUTES,
        &config,
        WDF_NO_HANDLE);
}

NTSTATUS RkvmEvtDeviceAdd(_In_ WDFDRIVER Driver, _Inout_ PWDFDEVICE_INIT DeviceInit)
{
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_FILEOBJECT_CONFIG fileConfig;
    WDF_IO_QUEUE_CONFIG queueConfig;
    WDFDEVICE device;
    PRKVM_DEVICE_CONTEXT context;
    VHF_CONFIG vhfConfig;
    UNICODE_STRING deviceName;
    UNICODE_STRING symbolicLink;
    NTSTATUS status;

    UNREFERENCED_PARAMETER(Driver);
    PAGED_CODE();

    RtlInitUnicodeString(&deviceName, L"\\Device\\RkvmVirtualHid");
    status = WdfDeviceInitAssignName(DeviceInit, &deviceName);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    status = WdfDeviceInitAssignSDDLString(DeviceInit, &SDDL_DEVOBJ_SYS_ALL_ADM_ALL);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    WdfDeviceInitSetDeviceType(DeviceInit, FILE_DEVICE_UNKNOWN);
    WdfDeviceInitSetCharacteristics(DeviceInit, FILE_DEVICE_SECURE_OPEN, FALSE);
    WdfDeviceInitSetExclusive(DeviceInit, TRUE);
    WDF_FILEOBJECT_CONFIG_INIT(
        &fileConfig,
        WDF_NO_EVENT_CALLBACK,
        WDF_NO_EVENT_CALLBACK,
        RkvmEvtFileCleanup);
    WdfDeviceInitSetFileObjectConfig(DeviceInit, &fileConfig, WDF_NO_OBJECT_ATTRIBUTES);

    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, RKVM_DEVICE_CONTEXT);
    attributes.EvtCleanupCallback = RkvmEvtDeviceCleanup;
    status = WdfDeviceCreate(&DeviceInit, &attributes, &device);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    context = RkvmGetDeviceContext(device);
    context->VhfHandle = NULL;

    RtlInitUnicodeString(&symbolicLink, L"\\DosDevices\\RkvmVirtualHid");
    status = WdfDeviceCreateSymbolicLink(device, &symbolicLink);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queueConfig, WdfIoQueueDispatchSequential);
    queueConfig.EvtIoWrite = RkvmEvtIoWrite;
    status = WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, WDF_NO_HANDLE);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    VHF_CONFIG_INIT(
        &vhfConfig,
        WdfDeviceWdmGetDeviceObject(device),
        (USHORT)sizeof(RkvmReportDescriptor),
        RkvmReportDescriptor);
    vhfConfig.VendorID = 0x1209;
    vhfConfig.ProductID = 0x4B56;
    vhfConfig.VersionNumber = 0x0001;

    status = VhfCreate(&vhfConfig, &context->VhfHandle);
    if (!NT_SUCCESS(status)) {
        context->VhfHandle = NULL;
        return status;
    }

    status = VhfStart(context->VhfHandle);
    if (!NT_SUCCESS(status)) {
        VhfDelete(context->VhfHandle, TRUE);
        context->VhfHandle = NULL;
    }
    return status;
}

VOID RkvmEvtDeviceCleanup(_In_ WDFOBJECT DeviceObject)
{
    PRKVM_DEVICE_CONTEXT context = RkvmGetDeviceContext(DeviceObject);

    PAGED_CODE();
    if (context->VhfHandle != NULL) {
        VhfDelete(context->VhfHandle, TRUE);
        context->VhfHandle = NULL;
    }
}

VOID RkvmEvtFileCleanup(_In_ WDFFILEOBJECT FileObject)
{
    WDFDEVICE device = WdfFileObjectGetDevice(FileObject);
    PRKVM_DEVICE_CONTEXT context = RkvmGetDeviceContext(device);
    UCHAR keyboardNeutral[RKVM_KEYBOARD_REPORT_SIZE] = {RKVM_KEYBOARD_REPORT_ID};
    UCHAR mouseNeutral[RKVM_MOUSE_REPORT_SIZE] = {RKVM_MOUSE_REPORT_ID};
    UCHAR consumerNeutral[RKVM_CONSUMER_REPORT_SIZE] = {RKVM_CONSUMER_REPORT_ID};
    UCHAR systemNeutral[RKVM_SYSTEM_REPORT_SIZE] = {RKVM_SYSTEM_REPORT_ID};

    PAGED_CODE();
    (void)RkvmSubmitReport(context, keyboardNeutral, sizeof(keyboardNeutral));
    (void)RkvmSubmitReport(context, mouseNeutral, sizeof(mouseNeutral));
    (void)RkvmSubmitReport(context, consumerNeutral, sizeof(consumerNeutral));
    (void)RkvmSubmitReport(context, systemNeutral, sizeof(systemNeutral));
}

VOID RkvmEvtIoWrite(
    _In_ WDFQUEUE Queue,
    _In_ WDFREQUEST Request,
    _In_ size_t Length)
{
    WDFDEVICE device = WdfIoQueueGetDevice(Queue);
    PRKVM_DEVICE_CONTEXT context = RkvmGetDeviceContext(device);
    UCHAR *report = NULL;
    NTSTATUS status;

    status = WdfRequestRetrieveInputBuffer(Request, 1, (PVOID *)&report, NULL);
    if (!NT_SUCCESS(status)) {
        WdfRequestComplete(Request, status);
        return;
    }
    if (!RkvmValidReport(report, Length)) {
        WdfRequestComplete(Request, STATUS_INVALID_BUFFER_SIZE);
        return;
    }
    status = RkvmSubmitReport(context, report, Length);
    WdfRequestCompleteWithInformation(Request, status, NT_SUCCESS(status) ? Length : 0);
}
