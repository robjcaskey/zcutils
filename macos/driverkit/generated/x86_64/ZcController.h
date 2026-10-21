/* iig(DriverKit-427.100.5) generated from ZcController.iig */

/* ZcController.iig:1-2 */
#include <SCSIControllerDriverKit/IOUserSCSIParallelInterfaceController.h>  /* .iig include */
#include <DriverKit/IOUserClient.h>  /* .iig include */
/* source class ZcController ZcController.iig:3-37 */

#if __DOCUMENTATION__
#define KERNEL IIG_KERNEL

class ZcController : public IOUserSCSIParallelInterfaceController {
public:
    virtual bool init() override;
    virtual void free() override;
    virtual kern_return_t Start(IOService *provider) override;
    virtual kern_return_t Stop(IOService *provider) override;
    virtual kern_return_t NewUserClient(uint32_t type, IOUserClient **userClient) override;
    virtual void ParallelTaskCompletion ( OSAction * action TARGET, SCSIUserParallelResponse response ) override;
    virtual kern_return_t UserReportHBAHighestLogicalUnitNumber ( uint64_t * value ) override;
    virtual kern_return_t UserDoesHBASupportSCSIParallelFeature ( uint32_t theValue, bool * result ) override;
    virtual kern_return_t UserInitializeTargetForID ( SCSITargetIdentifier targetID ) override;
    virtual kern_return_t UserDoesHBAPerformAutoSense ( bool * result ) override;
    virtual kern_return_t UserDoesHBASupportMultiPathing ( bool * result ) override;
    virtual kern_return_t UserAbortTaskRequest ( uint64_t theT, uint64_t theL, uint64_t theQ, uint32_t * response ) override;
    virtual kern_return_t UserAbortTaskSetRequest ( uint64_t theT, uint64_t theL, uint32_t * response ) override;
    virtual kern_return_t UserClearACARequest ( uint64_t theT, uint64_t theL, uint32_t * response ) override;
    virtual kern_return_t UserClearTaskSetRequest ( uint64_t theT, uint64_t theL, uint32_t * response ) override;
    virtual kern_return_t UserLogicalUnitResetRequest ( uint64_t theT, uint64_t theL, uint32_t * response ) override;
    virtual kern_return_t UserTargetResetRequest ( uint64_t theT, uint32_t * response ) override;
    virtual kern_return_t UserReportInitiatorIdentifier ( uint64_t * id ) override;
    virtual kern_return_t UserReportHighestSupportedDeviceID ( uint64_t * id ) override;
    virtual kern_return_t UserReportMaximumTaskCount ( uint32_t * count ) override;
    virtual kern_return_t UserDoesHBAPerformDeviceManagement ( bool * result ) override;
    virtual kern_return_t UserInitializeController ( ) override;
    virtual kern_return_t UserStartController ( ) override;
    virtual kern_return_t UserProcessParallelTask ( SCSIUserParallelTask parallelRequest, uint32_t * response, OSAction * completion TYPE(ParallelTaskCompletion) ) override;
    virtual kern_return_t UserGetDMASpecification ( uint64_t * maxTransferSize, uint32_t * alignment, uint8_t * numAddressBits, DMAOutputSegmentType * segmentType ) override;
    virtual kern_return_t UserMapHBAData ( uint32_t * uniqueTaskID ) override;
    virtual kern_return_t UserMapBundledParallelTaskCommandAndResponseBuffers ( IOBufferMemoryDescriptor * parallelCommandIOMemoryDescriptor, IOBufferMemoryDescriptor * parallelResponseIOMemoryDescriptor ) override;
    virtual void BundledParallelTaskCompletion ( OSAction * action TARGET, const uint16_t parallelResponseSlotIndices[kMaxBundledParallelTasks], uint16_t parallelResponseSlotIndicesCount ) override;
    virtual void UserProcessBundledParallelTasks ( const uint16_t parallelRequestSlotIndices[kMaxBundledParallelTasks], uint16_t parallelRequestSlotIndicesCount, OSAction * completion TYPE(BundledParallelTaskCompletion) ) override;
    kern_return_t BrokerCall(uint64_t selector, IOUserClientMethodArguments *args, IOUserClient *owner) LOCALONLY;
    kern_return_t BrokerMemory(uint64_t type, uint64_t *options, IOMemoryDescriptor **memory, IOUserClient *owner) LOCALONLY;
    void Disconnect(IOUserClient *owner) LOCALONLY;
    void Finish(uint64_t bytes, uint8_t key, uint8_t asc) LOCALONLY;
};

#undef KERNEL
#else /* __DOCUMENTATION__ */

/* generated class ZcController ZcController.iig:3-37 */


#define ZcController_Start_Args \
        IOService * provider

#define ZcController_Stop_Args \
        IOService * provider

#define ZcController_NewUserClient_Args \
        uint32_t type, \
        IOUserClient ** userClient

#define ZcController_ParallelTaskCompletion_Args \
        OSAction * action, \
        SCSIUserParallelResponse response

#define ZcController_UserReportHBAHighestLogicalUnitNumber_Args \
        uint64_t * value

#define ZcController_UserDoesHBASupportSCSIParallelFeature_Args \
        uint32_t theValue, \
        bool * result

#define ZcController_UserInitializeTargetForID_Args \
        SCSITargetIdentifier targetID

#define ZcController_UserDoesHBAPerformAutoSense_Args \
        bool * result

#define ZcController_UserDoesHBASupportMultiPathing_Args \
        bool * result

#define ZcController_UserAbortTaskRequest_Args \
        uint64_t theT, \
        uint64_t theL, \
        uint64_t theQ, \
        uint32_t * response

#define ZcController_UserAbortTaskSetRequest_Args \
        uint64_t theT, \
        uint64_t theL, \
        uint32_t * response

#define ZcController_UserClearACARequest_Args \
        uint64_t theT, \
        uint64_t theL, \
        uint32_t * response

#define ZcController_UserClearTaskSetRequest_Args \
        uint64_t theT, \
        uint64_t theL, \
        uint32_t * response

#define ZcController_UserLogicalUnitResetRequest_Args \
        uint64_t theT, \
        uint64_t theL, \
        uint32_t * response

#define ZcController_UserTargetResetRequest_Args \
        uint64_t theT, \
        uint32_t * response

#define ZcController_UserReportInitiatorIdentifier_Args \
        uint64_t * id

#define ZcController_UserReportHighestSupportedDeviceID_Args \
        uint64_t * id

#define ZcController_UserReportMaximumTaskCount_Args \
        uint32_t * count

#define ZcController_UserDoesHBAPerformDeviceManagement_Args \
        bool * result

#define ZcController_UserInitializeController_Args \


#define ZcController_UserStartController_Args \


#define ZcController_UserProcessParallelTask_Args \
        SCSIUserParallelTask parallelRequest, \
        uint32_t * response, \
        OSAction * completion

#define ZcController_UserGetDMASpecification_Args \
        uint64_t * maxTransferSize, \
        uint32_t * alignment, \
        uint8_t * numAddressBits, \
        DMAOutputSegmentType * segmentType

#define ZcController_UserMapHBAData_Args \
        uint32_t * uniqueTaskID

#define ZcController_UserMapBundledParallelTaskCommandAndResponseBuffers_Args \
        IOBufferMemoryDescriptor * parallelCommandIOMemoryDescriptor, \
        IOBufferMemoryDescriptor * parallelResponseIOMemoryDescriptor

#define ZcController_BundledParallelTaskCompletion_Args \
        OSAction * action, \
        const unsigned short * parallelResponseSlotIndices, \
        uint16_t parallelResponseSlotIndicesCount

#define ZcController_UserProcessBundledParallelTasks_Args \
        const unsigned short * parallelRequestSlotIndices, \
        uint16_t parallelRequestSlotIndicesCount, \
        OSAction * completion

#define ZcController_Methods \
\
public:\
\
    virtual kern_return_t\
    Dispatch(const IORPC rpc) APPLE_KEXT_OVERRIDE;\
\
    static kern_return_t\
    _Dispatch(ZcController * self, const IORPC rpc);\
\
    kern_return_t\
    BrokerCall(\
        uint64_t selector,\
        IOUserClientMethodArguments * args,\
        IOUserClient * owner);\
\
    kern_return_t\
    BrokerMemory(\
        uint64_t type,\
        uint64_t * options,\
        IOMemoryDescriptor ** memory,\
        IOUserClient * owner);\
\
    void\
    Disconnect(\
        IOUserClient * owner);\
\
    void\
    Finish(\
        uint64_t bytes,\
        uint8_t key,\
        uint8_t asc);\
\
\
protected:\
    /* _Impl methods */\
\
    kern_return_t\
    Start_Impl(IOService_Start_Args);\
\
    kern_return_t\
    Stop_Impl(IOService_Stop_Args);\
\
    kern_return_t\
    NewUserClient_Impl(IOService_NewUserClient_Args);\
\
    void\
    ParallelTaskCompletion_Impl(IOUserSCSIParallelInterfaceController_ParallelTaskCompletion_Args);\
\
    kern_return_t\
    UserReportHBAHighestLogicalUnitNumber_Impl(IOUserSCSIParallelInterfaceController_UserReportHBAHighestLogicalUnitNumber_Args);\
\
    kern_return_t\
    UserDoesHBASupportSCSIParallelFeature_Impl(IOUserSCSIParallelInterfaceController_UserDoesHBASupportSCSIParallelFeature_Args);\
\
    kern_return_t\
    UserInitializeTargetForID_Impl(IOUserSCSIParallelInterfaceController_UserInitializeTargetForID_Args);\
\
    kern_return_t\
    UserDoesHBAPerformAutoSense_Impl(IOUserSCSIParallelInterfaceController_UserDoesHBAPerformAutoSense_Args);\
\
    kern_return_t\
    UserDoesHBASupportMultiPathing_Impl(IOUserSCSIParallelInterfaceController_UserDoesHBASupportMultiPathing_Args);\
\
    kern_return_t\
    UserAbortTaskRequest_Impl(IOUserSCSIParallelInterfaceController_UserAbortTaskRequest_Args);\
\
    kern_return_t\
    UserAbortTaskSetRequest_Impl(IOUserSCSIParallelInterfaceController_UserAbortTaskSetRequest_Args);\
\
    kern_return_t\
    UserClearACARequest_Impl(IOUserSCSIParallelInterfaceController_UserClearACARequest_Args);\
\
    kern_return_t\
    UserClearTaskSetRequest_Impl(IOUserSCSIParallelInterfaceController_UserClearTaskSetRequest_Args);\
\
    kern_return_t\
    UserLogicalUnitResetRequest_Impl(IOUserSCSIParallelInterfaceController_UserLogicalUnitResetRequest_Args);\
\
    kern_return_t\
    UserTargetResetRequest_Impl(IOUserSCSIParallelInterfaceController_UserTargetResetRequest_Args);\
\
    kern_return_t\
    UserReportInitiatorIdentifier_Impl(IOUserSCSIParallelInterfaceController_UserReportInitiatorIdentifier_Args);\
\
    kern_return_t\
    UserReportHighestSupportedDeviceID_Impl(IOUserSCSIParallelInterfaceController_UserReportHighestSupportedDeviceID_Args);\
\
    kern_return_t\
    UserReportMaximumTaskCount_Impl(IOUserSCSIParallelInterfaceController_UserReportMaximumTaskCount_Args);\
\
    kern_return_t\
    UserDoesHBAPerformDeviceManagement_Impl(IOUserSCSIParallelInterfaceController_UserDoesHBAPerformDeviceManagement_Args);\
\
    kern_return_t\
    UserInitializeController_Impl(IOUserSCSIParallelInterfaceController_UserInitializeController_Args);\
\
    kern_return_t\
    UserStartController_Impl(IOUserSCSIParallelInterfaceController_UserStartController_Args);\
\
    kern_return_t\
    UserProcessParallelTask_Impl(IOUserSCSIParallelInterfaceController_UserProcessParallelTask_Args);\
\
    kern_return_t\
    UserGetDMASpecification_Impl(IOUserSCSIParallelInterfaceController_UserGetDMASpecification_Args);\
\
    kern_return_t\
    UserMapHBAData_Impl(IOUserSCSIParallelInterfaceController_UserMapHBAData_Args);\
\
    kern_return_t\
    UserMapBundledParallelTaskCommandAndResponseBuffers_Impl(IOUserSCSIParallelInterfaceController_UserMapBundledParallelTaskCommandAndResponseBuffers_Args);\
\
    void\
    BundledParallelTaskCompletion_Impl(IOUserSCSIParallelInterfaceController_BundledParallelTaskCompletion_Args);\
\
    void\
    UserProcessBundledParallelTasks_Impl(IOUserSCSIParallelInterfaceController_UserProcessBundledParallelTasks_Args);\
\
\
public:\
    /* _Invoke methods */\
\


#define ZcController_KernelMethods \
\
protected:\
    /* _Impl methods */\
\


#define ZcController_VirtualMethods \
\
public:\
\
    virtual bool\
    init(\
) APPLE_KEXT_OVERRIDE;\
\
    virtual void\
    free(\
) APPLE_KEXT_OVERRIDE;\
\


#if !KERNEL

extern OSMetaClass          * gZcControllerMetaClass;
extern const OSClassLoadInformation ZcController_Class;

class ZcControllerMetaClass : public OSMetaClass
{
public:
    virtual kern_return_t
    New(OSObject * instance) override;
    virtual kern_return_t
    Dispatch(const IORPC rpc) override;
};

#endif /* !KERNEL */

#if !KERNEL

class  ZcControllerInterface : public OSInterface
{
public:
};

struct ZcController_IVars;
struct ZcController_LocalIVars;

class ZcController : public IOUserSCSIParallelInterfaceController, public ZcControllerInterface
{
#if !KERNEL
    friend class ZcControllerMetaClass;
#endif /* !KERNEL */

#if !KERNEL
public:
#ifdef ZcController_DECLARE_IVARS
ZcController_DECLARE_IVARS
#else /* ZcController_DECLARE_IVARS */
    union
    {
        ZcController_IVars * ivars;
        ZcController_LocalIVars * lvars;
    };
#endif /* ZcController_DECLARE_IVARS */
#endif /* !KERNEL */

#if !KERNEL
    static OSMetaClass *
    sGetMetaClass() { return gZcControllerMetaClass; };
#endif /* KERNEL */

    using super = IOUserSCSIParallelInterfaceController;

#if !KERNEL
    ZcController_Methods
    ZcController_VirtualMethods
#endif /* !KERNEL */

};
#endif /* !KERNEL */


#endif /* !__DOCUMENTATION__ */

/* ZcController.iig:39- */
