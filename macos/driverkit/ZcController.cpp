// Native block edge only. No placement, RAID, network, or leaf media here.
#include "ZcController.h"
#include "ZcUserClient.h"
#include <DriverKit/IOLib.h>
#include <DriverKit/OSNumber.h>
#include <DriverKit/OSDictionary.h>
#include <DriverKit/IODispatchQueue.h>
#include <DriverKit/IOBufferMemoryDescriptor.h>
#include <DriverKit/IOKitKeys.h>
#include <string.h>
static constexpr uint64_t kBlock = 4096;
struct ZcController_IVars {
    IOBufferMemoryDescriptor *staging;
    IOBufferMemoryDescriptor *taskBuffer;
    IOUserClient *owner;
    OSAction *completion;
    SCSIUserParallelTask task;
    uint64_t stagingAddress, taskAddress, token, nextToken;
    uint32_t nextTask;
    bool online, delivered, created;
};
struct ZcUserClient_IVars { ZcController *controller; };
bool ZcController::init() {
    if (!super::init()) return false;
    ivars = IONewZero(ZcController_IVars, 1);
    return ivars != nullptr;
}
void ZcController::free() {
    if (ivars) {
        OSSafeReleaseNULL(ivars->taskBuffer);
        OSSafeReleaseNULL(ivars->staging);
        IODelete(ivars, ZcController_IVars, 1);
    }
    super::free();
}
kern_return_t IMPL(ZcController, Start) {
    auto ret=Start(provider,SUPERDISPATCH);
    if (ret!=kIOReturnSuccess) return ret;
    return RegisterService();
}
kern_return_t IMPL(ZcController, Stop) {
    ivars->online=false; Finish(0,2,0x3a);
    return Stop(provider,SUPERDISPATCH);
}
kern_return_t IMPL(ZcController, UserInitializeController) {
    auto constraints=OSDictionary::withCapacity(7);
    if (!constraints) return kIOReturnNoMemory;
    const char *keys[]={kIOMaximumSegmentCountReadKey,kIOMaximumSegmentCountWriteKey,
        kIOMaximumSegmentByteCountReadKey,kIOMaximumSegmentByteCountWriteKey,
        kIOMinimumSegmentAlignmentByteCountKey,kIOMaximumSegmentAddressableBitCountKey,
        kIOMinimumHBADataAlignmentMaskKey};
    // One 4096-byte task, aligned within a page on both supported architectures.
    const uint64_t values[]={1,1,kBlock,kBlock,kBlock,64,0};
    for (unsigned i=0;i<7;++i) {
        auto n=OSNumber::withNumber(values[i],64);
        if (!n) {constraints->release();return kIOReturnNoMemory;}
        constraints->setObject(keys[i],n);n->release();
    }
    auto ret=UserReportHBAConstraints(constraints);constraints->release();
    if (ret!=kIOReturnSuccess) return ret;
    ivars->nextToken=1;ivars->nextTask=1;
    return kIOReturnSuccess;
}
kern_return_t IMPL(ZcController, UserStartController) { return kIOReturnSuccess; }
kern_return_t IMPL(ZcController, NewUserClient) {
    if (type!=0 || ivars->owner) return kIOReturnExclusiveAccess;
    IOService *service=nullptr;
    auto ret=Create(this,"ZcUserClientProperties",&service);
    if (ret!=kIOReturnSuccess) return ret;
    auto client=OSDynamicCast(IOUserClient,service);
    if (!client) {service->release();return kIOReturnBadArgument;}
    // A new owner gets new pages. Old mappings cannot observe a later session.
    ret=IOBufferMemoryDescriptor::Create(kIOMemoryDirectionInOut,16384,16384,&ivars->staging);
    if (ret!=kIOReturnSuccess) {service->release();return ret;}
    IOAddressSegment range={};ret=ivars->staging->GetAddressRange(&range);
    if (ret!=kIOReturnSuccess || range.length<kBlock) {
        OSSafeReleaseNULL(ivars->staging);service->release();return kIOReturnNoMemory;
    }
    ivars->stagingAddress=range.address;
    ivars->owner=client;*userClient=client;
    return kIOReturnSuccess;
}
kern_return_t ZcController::BrokerMemory(uint64_t type,uint64_t *options,IOMemoryDescriptor **memory,IOUserClient *owner) {
    if (owner!=ivars->owner || type!=0 || !ivars->staging) return kIOReturnNotPermitted;
    *options=0;ivars->staging->retain();*memory=ivars->staging;return kIOReturnSuccess;
}
void ZcController::Finish(uint64_t bytes,uint8_t key,uint8_t asc) {
    if (!ivars->completion) return;
    SCSIUserParallelResponse result={};
    result.version=kScsiUserParallelTaskResponseCurrentVersion1;
    result.fTargetID=ivars->task.fTargetID;
    result.fControllerTaskIdentifier=ivars->task.fControllerTaskIdentifier;
    result.fServiceResponse=kSCSIServiceResponse_TASK_COMPLETE;
    result.fCompletionStatus=key?kSCSITaskStatus_CHECK_CONDITION:kSCSITaskStatus_GOOD;
    result.fBytesTransferred=key?0:bytes;
    if (key) { result.fSenseLength=18;result.fSenseBuffer[0]=0x70;result.fSenseBuffer[2]=key;result.fSenseBuffer[7]=10;result.fSenseBuffer[12]=asc; }
    if (!key && ivars->task.fTransferDirection==kSCSIDataTransfer_FromTargetToInitiator && bytes)
        memcpy(reinterpret_cast<void *>(ivars->taskAddress),reinterpret_cast<void *>(ivars->stagingAddress),bytes);
    auto completion=ivars->completion;
    ivars->completion=nullptr;ivars->token=0;ivars->delivered=false;
    OSSafeReleaseNULL(ivars->taskBuffer);ivars->taskAddress=0;
    ParallelTaskCompletion(completion,result);completion->release();
}
void ZcController::Disconnect(IOUserClient *owner) {
    if (owner!=ivars->owner) return;
    ivars->online=false;Finish(0,2,0x3a);ivars->owner=nullptr;
    if (ivars->created) {UserDestroyTargetForID(0);ivars->created=false;}
    OSSafeReleaseNULL(ivars->staging);ivars->stagingAddress=0;
}
kern_return_t ZcController::BrokerCall(uint64_t selector,IOUserClientMethodArguments *args,IOUserClient *owner) {
    if (owner!=ivars->owner) return kIOReturnNotPermitted;
    if (args->completion || args->structureInput || args->structureInputDescriptor || args->structureOutputDescriptor || args->structureOutputMaximumSize) return kIOReturnBadArgument;
    if (selector==0) {
        if (args->scalarInputCount!=1 || args->scalarOutputCount || args->scalarInput[0]!=1 || ivars->online || ivars->created) return kIOReturnBadArgument;
        ivars->online=true;
        auto dict=OSDictionary::withCapacity(0);
        if (!dict) {ivars->online=false;return kIOReturnNoMemory;}
        auto ret=UserCreateTargetForID(0,dict);dict->release();
        if (ret!=kIOReturnSuccess) {ivars->online=false;return ret;}
        ivars->created=true;return kIOReturnSuccess;
    }
    if (!ivars->online) return kIOReturnNotReady;
    if (selector==1) {
        if (args->scalarInputCount || args->scalarOutputCount!=6) return kIOReturnBadArgument;
        memset(args->scalarOutput,0,6*sizeof(uint64_t));
        if (!ivars->completion || ivars->delivered) return kIOReturnSuccess;
        args->scalarOutput[0]=ivars->token;args->scalarOutput[1]=ivars->task.fRequestedTransferCount;
        auto direction=ivars->task.fTransferDirection;
        args->scalarOutput[2]=direction==kSCSIDataTransfer_FromTargetToInitiator?1:direction==kSCSIDataTransfer_FromInitiatorToTarget?2:0;
        args->scalarOutput[3]=ivars->task.fCommandSize;
        memcpy(&args->scalarOutput[4],ivars->task.fCommandDescriptorBlock,16);
        ivars->delivered=true;return kIOReturnSuccess;
    }
    if (selector==2) {
        if (args->scalarInputCount!=4 || args->scalarOutputCount || !ivars->completion || !ivars->delivered || args->scalarInput[0]!=ivars->token) return kIOReturnBadArgument;
        auto bytes=args->scalarInput[1],key=args->scalarInput[2],asc=args->scalarInput[3];
        if (bytes>ivars->task.fRequestedTransferCount || bytes>kBlock || key>15 || asc>255 || (key && bytes)) return kIOReturnBadArgument;
        Finish(bytes,static_cast<uint8_t>(key),static_cast<uint8_t>(asc));return kIOReturnSuccess;
    }
    return kIOReturnUnsupported;
}
kern_return_t IMPL(ZcController, UserProcessParallelTask) {
    *response=kSCSIServiceResponse_Request_In_Process;
    if (ivars->completion) {*response=kSCSIServiceResponse_SERVICE_DELIVERY_OR_TARGET_FAILURE;return kIOReturnBusy;}
    ivars->task=parallelRequest;ivars->completion=completion;completion->retain();ivars->token=ivars->nextToken++;
    if (!ivars->online || !ivars->nextToken || parallelRequest.fTargetID!=0 ||
        parallelRequest.fRequestedTransferCount>kBlock || parallelRequest.fCommandSize>16 ||
        parallelRequest.fCommandSize==0 || parallelRequest.fTransferDirection>kSCSIDataTransfer_FromTargetToInitiator) {
        Finish(0,5,0x24);return kIOReturnSuccess;
    }
    for (unsigned i=0;i<sizeof(parallelRequest.fLogicalUnitBytes);++i)
        if (parallelRequest.fLogicalUnitBytes[i]) {Finish(0,5,0x25);return kIOReturnSuccess;}
    if (parallelRequest.fRequestedTransferCount) {
        auto ret=UserGetDataBuffer(0,parallelRequest.fControllerTaskIdentifier,&ivars->taskBuffer);
        IOAddressSegment range={};
        if (ret!=kIOReturnSuccess || !ivars->taskBuffer || ivars->taskBuffer->GetAddressRange(&range)!=kIOReturnSuccess || range.length<parallelRequest.fRequestedTransferCount) {
            Finish(0,4,0x44);return kIOReturnSuccess;
        }
        ivars->taskAddress=range.address;
        if (parallelRequest.fTransferDirection==kSCSIDataTransfer_FromInitiatorToTarget)
            memcpy(reinterpret_cast<void *>(ivars->stagingAddress),reinterpret_cast<void *>(range.address),parallelRequest.fRequestedTransferCount);
    }
    return kIOReturnSuccess;
}
// Completion methods are TARGET RPCs implemented by the framework.
void IMPL(ZcController, ParallelTaskCompletion) {}
void IMPL(ZcController, BundledParallelTaskCompletion) {}
kern_return_t IMPL(ZcController, UserMapBundledParallelTaskCommandAndResponseBuffers) {return kIOReturnUnsupported;}
void IMPL(ZcController, UserProcessBundledParallelTasks) {}
kern_return_t IMPL(ZcController, UserGetDMASpecification) {*maxTransferSize=kBlock;*alignment=kBlock;*numAddressBits=64;*segmentType=kDMAOutputSegmentHost64;return kIOReturnSuccess;}
kern_return_t IMPL(ZcController, UserMapHBAData) {if (!ivars->nextTask) return kIOReturnNoResources;*uniqueTaskID=ivars->nextTask++;return kIOReturnSuccess;}
kern_return_t IMPL(ZcController, UserInitializeTargetForID) {return targetID==0?kIOReturnSuccess:kIOReturnBadArgument;}
kern_return_t IMPL(ZcController, UserReportHBAHighestLogicalUnitNumber) {*value=0;return kIOReturnSuccess;}
kern_return_t IMPL(ZcController, UserReportInitiatorIdentifier) {*id=1;return kIOReturnSuccess;}
kern_return_t IMPL(ZcController, UserReportHighestSupportedDeviceID) {*id=0;return kIOReturnSuccess;}
kern_return_t IMPL(ZcController, UserReportMaximumTaskCount) {*count=1;return kIOReturnSuccess;}
kern_return_t IMPL(ZcController, UserDoesHBAPerformDeviceManagement) {*result=true;return kIOReturnSuccess;}
kern_return_t IMPL(ZcController, UserDoesHBAPerformAutoSense) {*result=true;return kIOReturnSuccess;}
kern_return_t IMPL(ZcController, UserDoesHBASupportMultiPathing) {*result=false;return kIOReturnSuccess;}
kern_return_t IMPL(ZcController, UserDoesHBASupportSCSIParallelFeature) {*result=false;return kIOReturnSuccess;}
// A reset fences the broker session; a late completion can never touch reused buffers.
#define ZC_RESET(name) kern_return_t IMPL(ZcController,name) {ivars->online=false;Finish(0,0xb,0x47);*response=0;return kIOReturnSuccess;}
ZC_RESET(UserAbortTaskRequest)
ZC_RESET(UserAbortTaskSetRequest)
ZC_RESET(UserClearACARequest)
ZC_RESET(UserClearTaskSetRequest)
ZC_RESET(UserLogicalUnitResetRequest)
ZC_RESET(UserTargetResetRequest)

bool ZcUserClient::init() {
    if (!super::init()) return false;
    ivars=IONewZero(ZcUserClient_IVars,1);return ivars!=nullptr;
}
void ZcUserClient::free() {
    if (ivars) IODelete(ivars,ZcUserClient_IVars,1);
    super::free();
}
kern_return_t IMPL(ZcUserClient, Start) {
    auto ret=Start(provider,SUPERDISPATCH);if (ret!=kIOReturnSuccess) return ret;
    ivars->controller=OSDynamicCast(ZcController,provider);if (!ivars->controller) return kIOReturnBadArgument;
    // All driver state, userclient calls, and disconnects share one serial queue.
    IODispatchQueue *queue=nullptr;ret=provider->CopyDispatchQueue(kIOServiceDefaultQueueName,&queue);
    if (ret!=kIOReturnSuccess) return ret;
    ret=SetDispatchQueue(kIOServiceDefaultQueueName,queue);
    if (ret==kIOReturnSuccess) ret=SetDispatchQueue(kIOUserClientQueueNameExternalMethod,queue);
    queue->release();return ret;
}
kern_return_t IMPL(ZcUserClient, Stop) {
    if (ivars && ivars->controller) ivars->controller->Disconnect(this);
    return Stop(provider,SUPERDISPATCH);
}
kern_return_t ZcUserClient::ExternalMethod(uint64_t selector,IOUserClientMethodArguments *args,const IOUserClientMethodDispatch *,OSObject *,void *) {
    return ivars->controller->BrokerCall(selector,args,this);
}
kern_return_t IMPL(ZcUserClient, CopyClientMemoryForType) {return ivars->controller->BrokerMemory(type,options,memory,this);}
