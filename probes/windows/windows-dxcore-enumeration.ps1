$ErrorActionPreference='Stop'
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class DxCoreCheck {
    [DllImport("dxcore.dll")] static extern int DXCoreCreateAdapterFactory(ref Guid iid, out IntPtr factory);
    delegate int CreateList(IntPtr self,uint count,ref Guid attribute,ref Guid iid,out IntPtr list);
    delegate uint Count(IntPtr self);
    delegate int GetAdapter(IntPtr self,uint index,ref Guid iid,out IntPtr adapter);
    delegate int Property(IntPtr self,uint key,UIntPtr size,IntPtr value);
    static T Method<T>(IntPtr obj,int slot) where T:class {
        IntPtr proc=Marshal.ReadIntPtr(Marshal.ReadIntPtr(obj),slot*IntPtr.Size);
        return Marshal.GetDelegateForFunctionPointer(proc,typeof(T)) as T;
    }
    public static void Run() {
        var factoryId=new Guid("78ee5945-c36e-4b13-a669-005dd11c0f06");
        var listId=new Guid("526c7776-40e9-459b-b711-f32ad76dfc28");
        var adapterId=new Guid("f0db4c7f-fe5a-42a2-bd62-f2a6cf6fc83e");
        IntPtr factory; Marshal.ThrowExceptionForHR(DXCoreCreateAdapterFactory(ref factoryId,out factory));
        try {
            foreach(string name in new[]{"D3D11","D3D12","GPU"}) {
                var attr=new Guid(name=="D3D11"?"8c47866b-7583-450d-f0f0-6bada895af4b":name=="D3D12"?"0c9ece4d-2f6e-4f01-8c96-e89e331b47b1":"b69eb219-3ded-4464-979f-a00bd4687006");
                IntPtr list; Marshal.ThrowExceptionForHR(Method<CreateList>(factory,3)(factory,1,ref attr,ref listId,out list));
                try {
                    uint count=Method<Count>(list,4)(list); if(count>64) throw new Exception("Unexpected enumeration size");
                    Console.WriteLine("filter={0} count={1}",name,count);
                    for(uint i=0;i<count;i++) {
                        IntPtr adapter; Marshal.ThrowExceptionForHR(Method<GetAdapter>(list,3)(list,i,ref adapterId,out adapter));
                        IntPtr buf=Marshal.AllocHGlobal(16);
                        try {
                            var read=Method<Property>(adapter,6);
                            foreach(uint key in new uint[]{0,3,7,11}) {
                                uint bytes=key==11?1u:key==3?16u:8u;
                                int hr=read(adapter,key,(UIntPtr)bytes,buf);
                                if(hr<0) { Console.WriteLine("  property={0} HRESULT={1:X8}",key,hr);continue; }
                                if(key==3) Console.WriteLine("  pci={0:X4}:{1:X4}",Marshal.ReadInt32(buf),Marshal.ReadInt32(buf,4));
                                else if(key==11) Console.WriteLine("  hardware={0}",Marshal.ReadByte(buf));
                                else Console.WriteLine("  {0}={1}",key==0?"luid":"dedicated_bytes",Marshal.ReadInt64(buf));
                            }
                        } finally {Marshal.FreeHGlobal(buf);Marshal.Release(adapter);}
                    }
                } finally {Marshal.Release(list);}
            }
        } finally {Marshal.Release(factory);}
    }
}
'@
[DxCoreCheck]::Run()
