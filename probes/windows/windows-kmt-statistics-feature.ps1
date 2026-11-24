$ErrorActionPreference = 'Stop'
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class GpuCurrentMode {
    [StructLayout(LayoutKind.Sequential)] struct Adapter { public uint Handle; public int Low, High; public uint Sources; public int PreciseRegions; }
    [StructLayout(LayoutKind.Sequential)] struct Enumeration { public uint Count; public IntPtr Adapters; }
    [StructLayout(LayoutKind.Sequential)] struct Query { public uint Handle, Kind; public IntPtr Data; public uint Size; }
    [DllImport("gdi32.dll")] static extern int D3DKMTEnumAdapters2(ref Enumeration request);
    [DllImport("gdi32.dll")] static extern int D3DKMTQueryAdapterInfo(ref Query request);
    [DllImport("gdi32.dll")] static extern int D3DKMTCloseAdapter(ref uint handle);
    [StructLayout(LayoutKind.Sequential)] struct Feature { public uint Adapter, FeatureId; public ushort Version, Flags; }
    [DllImport("gdi32.dll")] static extern int D3DKMTIsFeatureEnabled(ref Feature feature);
    public static void Run() {
        const int maxAdapters=32, size=48;
        int stride=Marshal.SizeOf(typeof(Adapter));
        var adapters=Marshal.AllocHGlobal(maxAdapters*stride);
        var metadata=Marshal.AllocHGlobal(size);
        try {
            var enumeration=new Enumeration { Count=maxAdapters, Adapters=adapters };
            int status=D3DKMTEnumAdapters2(ref enumeration);
            if(status!=0 || enumeration.Count>maxAdapters) throw new InvalidOperationException(String.Format("Adapter enumeration status 0x{0:X8}",status));
            for(int i=0;i<enumeration.Count;i++) {
                var adapter=(Adapter)Marshal.PtrToStructure(IntPtr.Add(adapters,i*stride),typeof(Adapter));
                try {
                    Marshal.Copy(new byte[size],0,metadata,size);
                    var ids = new Query { Handle=adapter.Handle, Kind=31, Data=metadata, Size=28 };
                    int idStatus = D3DKMTQueryAdapterInfo(ref ids);
                    Console.WriteLine("LUID={0:X8}:{1:X8}",adapter.High,adapter.Low);
                    Console.WriteLine("Adapter {0} PCI query=0x{1:X8} vendor={2:X4} device={3:X4} sources={4}",i,idStatus,Marshal.ReadInt32(metadata,4),Marshal.ReadInt32(metadata,8),adapter.Sources);
                    Marshal.Copy(new byte[size],0,metadata,size);
                    var feature=new Feature { Adapter=adapter.Handle, FeatureId=0x10000000 };
                    status=D3DKMTIsFeatureEnabled(ref feature);
                    Console.WriteLine("  query_statistics_extensions status=0x{0:X8} version={1} flags=0x{2:X4}",status,feature.Version,feature.Flags);
                } finally { D3DKMTCloseAdapter(ref adapter.Handle); }
            }
        } finally { Marshal.FreeHGlobal(metadata); Marshal.FreeHGlobal(adapters); }
    }
}
'@
[GpuCurrentMode]::Run()
