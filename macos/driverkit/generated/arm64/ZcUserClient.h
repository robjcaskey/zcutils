/* iig(DriverKit-427.100.5) generated from ZcUserClient.iig */

#include <DriverKit/IOUserClient.h>  /* .iig include */
/* source class ZcUserClient ZcUserClient.iig:2-10 */

#if __DOCUMENTATION__
#define KERNEL IIG_KERNEL

class ZcUserClient : public IOUserClient {
public:
    virtual bool init() override;
    virtual void free() override;
    virtual kern_return_t Start(IOService *provider) override;
    virtual kern_return_t Stop(IOService *provider) override;
    virtual kern_return_t ExternalMethod(uint64_t selector, IOUserClientMethodArguments *args,
        const IOUserClientMethodDispatch *dispatch, OSObject *target, void *reference) override;
    virtual kern_return_t CopyClientMemoryForType(uint64_t type, uint64_t *options, IOMemoryDescriptor **memory) override;
};

#undef KERNEL
#else /* __DOCUMENTATION__ */

/* generated class ZcUserClient ZcUserClient.iig:2-10 */


#define ZcUserClient_Start_Args \
        IOService * provider

#define ZcUserClient_Stop_Args \
        IOService * provider

#define ZcUserClient_CopyClientMemoryForType_Args \
        uint64_t type, \
        uint64_t * options, \
        IOMemoryDescriptor ** memory

#define ZcUserClient_Methods \
\
public:\
\
    virtual kern_return_t\
    Dispatch(const IORPC rpc) APPLE_KEXT_OVERRIDE;\
\
    static kern_return_t\
    _Dispatch(ZcUserClient * self, const IORPC rpc);\
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
    CopyClientMemoryForType_Impl(IOUserClient_CopyClientMemoryForType_Args);\
\
\
public:\
    /* _Invoke methods */\
\


#define ZcUserClient_KernelMethods \
\
protected:\
    /* _Impl methods */\
\


#define ZcUserClient_VirtualMethods \
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
    virtual kern_return_t\
    ExternalMethod(\
        uint64_t selector,\
        IOUserClientMethodArguments * args,\
        const IOUserClientMethodDispatch * dispatch,\
        OSObject * target,\
        void * reference) APPLE_KEXT_OVERRIDE;\
\


#if !KERNEL

extern OSMetaClass          * gZcUserClientMetaClass;
extern const OSClassLoadInformation ZcUserClient_Class;

class ZcUserClientMetaClass : public OSMetaClass
{
public:
    virtual kern_return_t
    New(OSObject * instance) override;
    virtual kern_return_t
    Dispatch(const IORPC rpc) override;
};

#endif /* !KERNEL */

#if !KERNEL

class  ZcUserClientInterface : public OSInterface
{
public:
};

struct ZcUserClient_IVars;
struct ZcUserClient_LocalIVars;

class ZcUserClient : public IOUserClient, public ZcUserClientInterface
{
#if !KERNEL
    friend class ZcUserClientMetaClass;
#endif /* !KERNEL */

#if !KERNEL
public:
#ifdef ZcUserClient_DECLARE_IVARS
ZcUserClient_DECLARE_IVARS
#else /* ZcUserClient_DECLARE_IVARS */
    union
    {
        ZcUserClient_IVars * ivars;
        ZcUserClient_LocalIVars * lvars;
    };
#endif /* ZcUserClient_DECLARE_IVARS */
#endif /* !KERNEL */

#if !KERNEL
    static OSMetaClass *
    sGetMetaClass() { return gZcUserClientMetaClass; };
#endif /* KERNEL */

    using super = IOUserClient;

#if !KERNEL
    ZcUserClient_Methods
    ZcUserClient_VirtualMethods
#endif /* !KERNEL */

};
#endif /* !KERNEL */


#endif /* !__DOCUMENTATION__ */

/* ZcUserClient.iig:12- */
