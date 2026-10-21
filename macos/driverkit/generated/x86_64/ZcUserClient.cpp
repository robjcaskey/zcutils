/* iig(DriverKit-427.100.5 Apr 30 2025 01:56:59) generated from ZcUserClient.iig */

#undef	IIG_IMPLEMENTATION
#define	IIG_IMPLEMENTATION 	ZcUserClient.iig

#if KERNEL
#include <libkern/c++/OSString.h>
#else
#include <DriverKit/DriverKit.h>
#endif /* KERNEL */
#include <DriverKit/IOReturn.h>
#include "ZcUserClient.h"


#if __has_builtin(__builtin_load_member_function_pointer)
#define SimpleMemberFunctionCast(cfnty, self, func) (cfnty)__builtin_load_member_function_pointer(self, func)
#else
#define SimpleMemberFunctionCast(cfnty, self, func) ({ union { typeof(func) memfun; cfnty cfun; } pair; pair.memfun = func; pair.cfun; })
#endif


#if !KERNEL
extern OSMetaClass * gOSContainerMetaClass;
extern OSMetaClass * gOSDataMetaClass;
extern OSMetaClass * gOSNumberMetaClass;
extern OSMetaClass * gOSBooleanMetaClass;
extern OSMetaClass * gOSDictionaryMetaClass;
extern OSMetaClass * gOSArrayMetaClass;
extern OSMetaClass * gOSSetMetaClass;
extern OSMetaClass * gOSOrderedSetMetaClass;
extern OSMetaClass * gIODispatchQueueMetaClass;
extern OSMetaClass * gOSStringMetaClass;
extern OSMetaClass * gIOServiceStateNotificationDispatchSourceMetaClass;
extern OSMetaClass * gIOMemoryMapMetaClass;
extern OSMetaClass * gOSAction_IOUserClient_KernelCompletionMetaClass;
#endif /* !KERNEL */

#if !KERNEL

#define ZcUserClient_QueueNames  "" \
    "\037IOUserClientQueueExternalMethod"

#define ZcUserClient_MethodNames  "" \
    "\017_ExternalMethod"

#define ZcUserClientMetaClass_MethodNames  ""

struct OSClassDescription_ZcUserClient_t
{
    OSClassDescription base;
    uint64_t           methodOptions[2 * 1];
    uint64_t           metaMethodOptions[2 * 0];
    char               queueNames[sizeof(ZcUserClient_QueueNames)];
    char               methodNames[sizeof(ZcUserClient_MethodNames)];
    char               metaMethodNames[sizeof(ZcUserClientMetaClass_MethodNames)];
};

const struct OSClassDescription_ZcUserClient_t
OSClassDescription_ZcUserClient =
{
    .base =
    {
        .descriptionSize         = sizeof(OSClassDescription_ZcUserClient_t),
        .name                    = "ZcUserClient",
        .superName               = "IOUserClient",
        .methodOptionsSize       = 2 * sizeof(uint64_t) * 1,
        .methodOptionsOffset     = __builtin_offsetof(struct OSClassDescription_ZcUserClient_t, methodOptions),
        .metaMethodOptionsSize   = 2 * sizeof(uint64_t) * 0,
        .metaMethodOptionsOffset = __builtin_offsetof(struct OSClassDescription_ZcUserClient_t, metaMethodOptions),
        .queueNamesSize       = sizeof(ZcUserClient_QueueNames),
        .queueNamesOffset     = __builtin_offsetof(struct OSClassDescription_ZcUserClient_t, queueNames),
        .methodNamesSize         = sizeof(ZcUserClient_MethodNames),
        .methodNamesOffset       = __builtin_offsetof(struct OSClassDescription_ZcUserClient_t, methodNames),
        .metaMethodNamesSize     = sizeof(ZcUserClientMetaClass_MethodNames),
        .metaMethodNamesOffset   = __builtin_offsetof(struct OSClassDescription_ZcUserClient_t, metaMethodNames),
        .flags                   = 0*kOSClassCanRemote,
        .resv1                   = {0},
    },
    .methodOptions =
    {
        IOUserClient__ExternalMethod_ID,
        0x0000000000000000,
    },
    .metaMethodOptions =
    {
    },
    .queueNames      = ZcUserClient_QueueNames,
    .methodNames     = ZcUserClient_MethodNames,
    .metaMethodNames = ZcUserClientMetaClass_MethodNames,
};

OSMetaClass * gZcUserClientMetaClass;

static kern_return_t
ZcUserClient_New(OSMetaClass * instance);

const OSClassLoadInformation
ZcUserClient_Class = 
{
    .description       = &OSClassDescription_ZcUserClient.base,
    .metaPointer       = &gZcUserClientMetaClass,
    .version           = 1,
    .instanceSize      = sizeof(ZcUserClient),

    .resv2             = {0},

    .New               = &ZcUserClient_New,
    .resv3             = {0},

};

extern const void * const
gZcUserClient_Declaration;
const void * const
gZcUserClient_Declaration
__attribute__((used,visibility("hidden"),section("__DATA_CONST,__osclassinfo,regular,no_dead_strip"),no_sanitize("address")))
    = &ZcUserClient_Class;

static kern_return_t
ZcUserClient_New(OSMetaClass * instance)
{
    if (!new(instance) ZcUserClientMetaClass) return (kIOReturnNoMemory);
    return (kIOReturnSuccess);
}

kern_return_t
ZcUserClientMetaClass::New(OSObject * instance)
{
    if (!new(instance) ZcUserClient) return (kIOReturnNoMemory);
    return (kIOReturnSuccess);
}

#endif /* !KERNEL */

#ifdef KERNEL
#define MESSAGE_CONTENT(__field) (messageContent->__field)
#else /* KERNEL */
#define MESSAGE_CONTENT(__field) (message->content.__field)
#endif /* KERNEL */

kern_return_t
ZcUserClient::Dispatch(const IORPC rpc)
{
    return _Dispatch(this, rpc);
}

kern_return_t
ZcUserClient::_Dispatch(ZcUserClient * self, const IORPC rpc)
{
    kern_return_t ret = kIOReturnUnsupported;
#ifdef KERNEL
    IORPCMessage * msg = rpc.kernelContent;
#else /* KERNEL */
    IORPCMessage * msg = IORPCMessageFromMach(rpc.message, false);
#endif /* KERNEL */

    switch (msg->msgid)
    {
        case IOService_Start_ID:
        {
            ret = IOService::Start_Invoke(rpc, self, SimpleMemberFunctionCast(IOService::Start_Handler, *self, &ZcUserClient::Start_Impl));
            break;
        }
        case IOService_Stop_ID:
        {
            ret = IOService::Stop_Invoke(rpc, self, SimpleMemberFunctionCast(IOService::Stop_Handler, *self, &ZcUserClient::Stop_Impl));
            break;
        }
        case IOUserClient_CopyClientMemoryForType_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserClient::CopyClientMemoryForType_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserClient::CopyClientMemoryForType_Handler, *self, &ZcUserClient::CopyClientMemoryForType_Impl));
            break;
        }

        default:
            ret = IOUserClient::_Dispatch(self, rpc);
            break;
    }

    return (ret);
}

#if KERNEL
kern_return_t
ZcUserClient::MetaClass::Dispatch(const IORPC rpc)
{
#else /* KERNEL */
kern_return_t
ZcUserClientMetaClass::Dispatch(const IORPC rpc)
{
#endif /* !KERNEL */

    kern_return_t ret = kIOReturnUnsupported;
#ifdef KERNEL
    IORPCMessage * msg = rpc.kernelContent;
#else /* KERNEL */
    IORPCMessage * msg = IORPCMessageFromMach(rpc.message, false);
#endif /* KERNEL */

    switch (msg->msgid)
    {

        default:
            ret = OSMetaClassBase::Dispatch(rpc);
            break;
    }

    return (ret);
}



