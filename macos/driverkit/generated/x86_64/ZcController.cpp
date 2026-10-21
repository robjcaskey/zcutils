/* iig(DriverKit-427.100.5 Apr 30 2025 01:56:59) generated from ZcController.iig */

#undef	IIG_IMPLEMENTATION
#define	IIG_IMPLEMENTATION 	ZcController.iig

#if KERNEL
#include <libkern/c++/OSString.h>
#else
#include <DriverKit/DriverKit.h>
#endif /* KERNEL */
#include <DriverKit/IOReturn.h>
#include "ZcController.h"


#if __has_builtin(__builtin_load_member_function_pointer)
#define SimpleMemberFunctionCast(cfnty, self, func) (cfnty)__builtin_load_member_function_pointer(self, func)
#else
#define SimpleMemberFunctionCast(cfnty, self, func) ({ union { typeof(func) memfun; cfnty cfun; } pair; pair.memfun = func; pair.cfun; })
#endif


#if !KERNEL
extern OSMetaClass * gOSDataMetaClass;
extern OSMetaClass * gOSNumberMetaClass;
extern OSMetaClass * gOSBooleanMetaClass;
extern OSMetaClass * gOSArrayMetaClass;
extern OSMetaClass * gOSSetMetaClass;
extern OSMetaClass * gOSOrderedSetMetaClass;
extern OSMetaClass * gIODispatchQueueMetaClass;
extern OSMetaClass * gIOServiceStateNotificationDispatchSourceMetaClass;
extern OSMetaClass * gOSStringMetaClass;
extern OSMetaClass * gIOMemoryMapMetaClass;
extern OSMetaClass * gOSAction_IOUserSCSIParallelInterfaceController_UserCompleteBundledParallelTaskMetaClass;
extern OSMetaClass * gOSAction_IOUserSCSIParallelInterfaceController_UserCompleteParallelTaskMetaClass;
extern OSMetaClass * gOSAction_IOUserClient_KernelCompletionMetaClass;
#endif /* !KERNEL */

#if !KERNEL

#define ZcController_QueueNames  "" \
    "\016AuxiliaryQueue"

#define ZcController_MethodNames  "" \
    "\025UserCreateTargetForID"

#define ZcControllerMetaClass_MethodNames  ""

struct OSClassDescription_ZcController_t
{
    OSClassDescription base;
    uint64_t           methodOptions[2 * 1];
    uint64_t           metaMethodOptions[2 * 0];
    char               queueNames[sizeof(ZcController_QueueNames)];
    char               methodNames[sizeof(ZcController_MethodNames)];
    char               metaMethodNames[sizeof(ZcControllerMetaClass_MethodNames)];
};

const struct OSClassDescription_ZcController_t
OSClassDescription_ZcController =
{
    .base =
    {
        .descriptionSize         = sizeof(OSClassDescription_ZcController_t),
        .name                    = "ZcController",
        .superName               = "IOUserSCSIParallelInterfaceController",
        .methodOptionsSize       = 2 * sizeof(uint64_t) * 1,
        .methodOptionsOffset     = __builtin_offsetof(struct OSClassDescription_ZcController_t, methodOptions),
        .metaMethodOptionsSize   = 2 * sizeof(uint64_t) * 0,
        .metaMethodOptionsOffset = __builtin_offsetof(struct OSClassDescription_ZcController_t, metaMethodOptions),
        .queueNamesSize       = sizeof(ZcController_QueueNames),
        .queueNamesOffset     = __builtin_offsetof(struct OSClassDescription_ZcController_t, queueNames),
        .methodNamesSize         = sizeof(ZcController_MethodNames),
        .methodNamesOffset       = __builtin_offsetof(struct OSClassDescription_ZcController_t, methodNames),
        .metaMethodNamesSize     = sizeof(ZcControllerMetaClass_MethodNames),
        .metaMethodNamesOffset   = __builtin_offsetof(struct OSClassDescription_ZcController_t, metaMethodNames),
        .flags                   = 0*kOSClassCanRemote,
        .resv1                   = {0},
    },
    .methodOptions =
    {
        IOUserSCSIParallelInterfaceController_UserCreateTargetForID_ID,
        0x0000000000000000,
    },
    .metaMethodOptions =
    {
    },
    .queueNames      = ZcController_QueueNames,
    .methodNames     = ZcController_MethodNames,
    .metaMethodNames = ZcControllerMetaClass_MethodNames,
};

OSMetaClass * gZcControllerMetaClass;

static kern_return_t
ZcController_New(OSMetaClass * instance);

const OSClassLoadInformation
ZcController_Class = 
{
    .description       = &OSClassDescription_ZcController.base,
    .metaPointer       = &gZcControllerMetaClass,
    .version           = 1,
    .instanceSize      = sizeof(ZcController),

    .resv2             = {0},

    .New               = &ZcController_New,
    .resv3             = {0},

};

extern const void * const
gZcController_Declaration;
const void * const
gZcController_Declaration
__attribute__((used,visibility("hidden"),section("__DATA_CONST,__osclassinfo,regular,no_dead_strip"),no_sanitize("address")))
    = &ZcController_Class;

static kern_return_t
ZcController_New(OSMetaClass * instance)
{
    if (!new(instance) ZcControllerMetaClass) return (kIOReturnNoMemory);
    return (kIOReturnSuccess);
}

kern_return_t
ZcControllerMetaClass::New(OSObject * instance)
{
    if (!new(instance) ZcController) return (kIOReturnNoMemory);
    return (kIOReturnSuccess);
}

#endif /* !KERNEL */

#ifdef KERNEL
#define MESSAGE_CONTENT(__field) (messageContent->__field)
#else /* KERNEL */
#define MESSAGE_CONTENT(__field) (message->content.__field)
#endif /* KERNEL */

kern_return_t
ZcController::Dispatch(const IORPC rpc)
{
    return _Dispatch(this, rpc);
}

kern_return_t
ZcController::_Dispatch(ZcController * self, const IORPC rpc)
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
            ret = IOService::Start_Invoke(rpc, self, SimpleMemberFunctionCast(IOService::Start_Handler, *self, &ZcController::Start_Impl));
            break;
        }
        case IOService_Stop_ID:
        {
            ret = IOService::Stop_Invoke(rpc, self, SimpleMemberFunctionCast(IOService::Stop_Handler, *self, &ZcController::Stop_Impl));
            break;
        }
        case IOService_NewUserClient_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOService::NewUserClient_Invoke(rpc, self, SimpleMemberFunctionCast(IOService::NewUserClient_Handler, *self, &ZcController::NewUserClient_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_ParallelTaskCompletion_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::ParallelTaskCompletion_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::ParallelTaskCompletion_Handler, *self, &ZcController::ParallelTaskCompletion_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserReportHBAHighestLogicalUnitNumber_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserReportHBAHighestLogicalUnitNumber_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserReportHBAHighestLogicalUnitNumber_Handler, *self, &ZcController::UserReportHBAHighestLogicalUnitNumber_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserDoesHBASupportSCSIParallelFeature_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserDoesHBASupportSCSIParallelFeature_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserDoesHBASupportSCSIParallelFeature_Handler, *self, &ZcController::UserDoesHBASupportSCSIParallelFeature_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserInitializeTargetForID_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserInitializeTargetForID_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserInitializeTargetForID_Handler, *self, &ZcController::UserInitializeTargetForID_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserDoesHBAPerformAutoSense_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserDoesHBAPerformAutoSense_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserDoesHBAPerformAutoSense_Handler, *self, &ZcController::UserDoesHBAPerformAutoSense_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserDoesHBASupportMultiPathing_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserDoesHBASupportMultiPathing_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserDoesHBASupportMultiPathing_Handler, *self, &ZcController::UserDoesHBASupportMultiPathing_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserAbortTaskRequest_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserAbortTaskRequest_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserAbortTaskRequest_Handler, *self, &ZcController::UserAbortTaskRequest_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserAbortTaskSetRequest_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserAbortTaskSetRequest_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserAbortTaskSetRequest_Handler, *self, &ZcController::UserAbortTaskSetRequest_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserClearACARequest_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserClearACARequest_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserClearACARequest_Handler, *self, &ZcController::UserClearACARequest_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserClearTaskSetRequest_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserClearTaskSetRequest_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserClearTaskSetRequest_Handler, *self, &ZcController::UserClearTaskSetRequest_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserLogicalUnitResetRequest_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserLogicalUnitResetRequest_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserLogicalUnitResetRequest_Handler, *self, &ZcController::UserLogicalUnitResetRequest_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserTargetResetRequest_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserTargetResetRequest_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserTargetResetRequest_Handler, *self, &ZcController::UserTargetResetRequest_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserReportInitiatorIdentifier_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserReportInitiatorIdentifier_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserReportInitiatorIdentifier_Handler, *self, &ZcController::UserReportInitiatorIdentifier_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserReportHighestSupportedDeviceID_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserReportHighestSupportedDeviceID_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserReportHighestSupportedDeviceID_Handler, *self, &ZcController::UserReportHighestSupportedDeviceID_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserReportMaximumTaskCount_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserReportMaximumTaskCount_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserReportMaximumTaskCount_Handler, *self, &ZcController::UserReportMaximumTaskCount_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserDoesHBAPerformDeviceManagement_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserDoesHBAPerformDeviceManagement_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserDoesHBAPerformDeviceManagement_Handler, *self, &ZcController::UserDoesHBAPerformDeviceManagement_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserInitializeController_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserInitializeController_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserInitializeController_Handler, *self, &ZcController::UserInitializeController_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserStartController_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserStartController_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserStartController_Handler, *self, &ZcController::UserStartController_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserProcessParallelTask_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserProcessParallelTask_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserProcessParallelTask_Handler, *self, &ZcController::UserProcessParallelTask_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserGetDMASpecification_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserGetDMASpecification_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserGetDMASpecification_Handler, *self, &ZcController::UserGetDMASpecification_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserMapHBAData_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserMapHBAData_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserMapHBAData_Handler, *self, &ZcController::UserMapHBAData_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserMapBundledParallelTaskCommandAndResponseBuffers_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserMapBundledParallelTaskCommandAndResponseBuffers_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserMapBundledParallelTaskCommandAndResponseBuffers_Handler, *self, &ZcController::UserMapBundledParallelTaskCommandAndResponseBuffers_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_BundledParallelTaskCompletion_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::BundledParallelTaskCompletion_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::BundledParallelTaskCompletion_Handler, *self, &ZcController::BundledParallelTaskCompletion_Impl));
            break;
        }
        case IOUserSCSIParallelInterfaceController_UserProcessBundledParallelTasks_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOUserSCSIParallelInterfaceController::UserProcessBundledParallelTasks_Invoke(rpc, self, SimpleMemberFunctionCast(IOUserSCSIParallelInterfaceController::UserProcessBundledParallelTasks_Handler, *self, &ZcController::UserProcessBundledParallelTasks_Impl));
            break;
        }

        default:
            ret = IOUserSCSIParallelInterfaceController::_Dispatch(self, rpc);
            break;
    }

    return (ret);
}

#if KERNEL
kern_return_t
ZcController::MetaClass::Dispatch(const IORPC rpc)
{
#else /* KERNEL */
kern_return_t
ZcControllerMetaClass::Dispatch(const IORPC rpc)
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



