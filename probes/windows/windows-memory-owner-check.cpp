// Use the same adapter in independent observer and resource-owner processes.
#include <windows.h>
#include <dxgi1_4.h>
#include <d3d11.h>
#include <cstdio>
#include <cstring>
#include <cstdlib>
#include <vector>

static bool report(IDXGIAdapter3 *adapter, const char *phase) {
    DXGI_QUERY_VIDEO_MEMORY_INFO info{};
    if (FAILED(adapter->QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &info))) return false;
    std::printf("%s %llu %llu\n", phase, info.CurrentUsage, info.Budget);
    return std::fflush(stdout) == 0;
}

int main(int argc, char **argv) {
    IDXGIFactory1 *factory = nullptr;
    if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) return 1;
    IDXGIAdapter1 *adapter = nullptr;
    DXGI_ADAPTER_DESC1 desc{};
    for (UINT i = 0; SUCCEEDED(factory->EnumAdapters1(i, &adapter)); ++i) {
        if (SUCCEEDED(adapter->GetDesc1(&desc)) && desc.VendorId == 0x10de) break;
        adapter->Release(); adapter = nullptr;
    }
    IDXGIAdapter3 *memory = nullptr;
    if (!adapter || FAILED(adapter->QueryInterface(IID_PPV_ARGS(&memory)))) return 2;
    if (argc == 1) return report(memory, "observer") ? 0 : 3;
    if (argc != 2 || std::strcmp(argv[1], "holder")) return 4;
    ID3D11Device *device = nullptr;
    ID3D11DeviceContext *context = nullptr;
    if (FAILED(D3D11CreateDevice(adapter,D3D_DRIVER_TYPE_UNKNOWN,nullptr,0,nullptr,0,D3D11_SDK_VERSION,&device,nullptr,&context))) return 5;
    if (!report(memory, "before") || std::getchar() == EOF) return 6;
    D3D11_BUFFER_DESC buffer_desc{};
    const char *setting = std::getenv("OWNER_PROBE_MIB");
    if (setting && std::strcmp(setting, "8") && std::strcmp(setting, "32")) return 10;
    buffer_desc.ByteWidth = (setting && !std::strcmp(setting, "8") ? 8u : 32u) * 1024u * 1024u;
    buffer_desc.Usage = D3D11_USAGE_DEFAULT;
    buffer_desc.BindFlags = D3D11_BIND_VERTEX_BUFFER;
    std::vector<unsigned char> initial_data(buffer_desc.ByteWidth, 0x5a);
    D3D11_SUBRESOURCE_DATA initial{};
    initial.pSysMem = initial_data.data();
    ID3D11Buffer *buffer = nullptr;
    if (FAILED(device->CreateBuffer(&buffer_desc,&initial,&buffer))) return 7;
    context->Flush();
    if (!report(memory,"held") || std::getchar()==EOF) return 8;
    buffer->Release(); context->Flush();
    const bool ok = report(memory,"released");
    context->Release(); device->Release(); memory->Release(); adapter->Release(); factory->Release();
    return ok ? 0 : 9;
}
