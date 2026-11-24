// Exercise public D3D12 residency operations on one owned 8 MiB buffer.
#include <windows.h>
#include <dxgi1_4.h>
#include <d3d12.h>
#include <cstdio>

int main() {
    IDXGIFactory1 *factory = nullptr;
    if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) return 1;
    IDXGIAdapter1 *adapter = nullptr;
    for (UINT i=0; SUCCEEDED(factory->EnumAdapters1(i,&adapter)); ++i) {
        DXGI_ADAPTER_DESC1 desc{};
        if (SUCCEEDED(adapter->GetDesc1(&desc)) && desc.VendorId==0x10de) break;
        adapter->Release(); adapter=nullptr;
    }
    factory->Release();
    if (!adapter) return 2;
    ID3D12Device *device=nullptr;
    HRESULT hr=D3D12CreateDevice(adapter,D3D_FEATURE_LEVEL_11_0,IID_PPV_ARGS(&device));
    adapter->Release();
    std::printf("D3D12CreateDevice=%08lx\n",static_cast<unsigned long>(hr));
    if (FAILED(hr)) return 3;
    D3D12_HEAP_PROPERTIES heap{};
    heap.Type=D3D12_HEAP_TYPE_DEFAULT;
    heap.CreationNodeMask=heap.VisibleNodeMask=1;
    D3D12_RESOURCE_DESC desc{};
    desc.Dimension=D3D12_RESOURCE_DIMENSION_BUFFER;
    desc.Width=8ull<<20;
    desc.Height=1; desc.DepthOrArraySize=1; desc.MipLevels=1;
    desc.SampleDesc.Count=1; desc.Layout=D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    ID3D12Resource *buffer=nullptr;
    hr=device->CreateCommittedResource(&heap,D3D12_HEAP_FLAG_NONE,&desc,D3D12_RESOURCE_STATE_COMMON,nullptr,IID_PPV_ARGS(&buffer));
    std::printf("CreateCommittedResource=%08lx\n",static_cast<unsigned long>(hr));
    if (FAILED(hr)) {device->Release();return 4;}
    ID3D12Pageable *object=buffer;
    HRESULT evicted=device->Evict(1,&object);
    std::printf("Evict=%08lx\n",static_cast<unsigned long>(evicted));
    HRESULT restored=FAILED(evicted)?evicted:device->MakeResident(1,&object);
    std::printf("MakeResident=%08lx\n",static_cast<unsigned long>(restored));
    buffer->Release();device->Release();
    return FAILED(restored)?5:0;
}
