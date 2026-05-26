#include "pxr/pxr.h"
#include "pxr/usd/ar/defineResolver.h"
#include "pxr/usd/ar/filesystemAsset.h"
#include "pxr/usd/ar/filesystemWritableAsset.h"
#include "pxr/usd/ar/inMemoryAsset.h"
#include "pxr/usd/ar/resolvedPath.h"
#include "pxr/usd/ar/resolver.h"

#include <algorithm>
#include <array>
#include <cctype>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <iostream>
#include <memory>
#include <sstream>
#include <string>
#include <vector>

PXR_NAMESPACE_OPEN_SCOPE

namespace {

constexpr const char* kAdsScheme = "ads:";

bool StartsWithAdsScheme(const std::string& value)
{
    return value.rfind(kAdsScheme, 0) == 0;
}

bool StartsWithHttpScheme(const std::string& value)
{
    return value.rfind("http://", 0) == 0 || value.rfind("https://", 0) == 0;
}

std::string GetEnv(const char* name, const std::string& fallback = "")
{
    if (const char* value = std::getenv(name)) {
        return value;
    }
    return fallback;
}

bool DebugEnabled()
{
    const std::string value = GetEnv("ADS_RESOLVER_DEBUG");
    return value == "1" || value == "true" || value == "TRUE" || value == "yes";
}

std::string Trim(std::string value)
{
    auto notSpace = [](unsigned char ch) { return !std::isspace(ch); };
    value.erase(value.begin(), std::find_if(value.begin(), value.end(), notSpace));
    value.erase(std::find_if(value.rbegin(), value.rend(), notSpace).base(), value.end());
    return value;
}

std::string NormalizeSlashes(std::string value)
{
    std::replace(value.begin(), value.end(), '\\', '/');
    return value;
}

std::string ShellQuote(const std::string& value)
{
#if defined(_WIN32)
    std::string quoted = "\"";
    unsigned backslashes = 0;
    for (char ch : value) {
        if (ch == '\\') {
            ++backslashes;
            continue;
        }
        if (ch == '"') {
            quoted.append(backslashes * 2 + 1, '\\');
            quoted.push_back('"');
        } else {
            quoted.append(backslashes, '\\');
            quoted.push_back(ch);
        }
        backslashes = 0;
    }
    quoted.append(backslashes * 2, '\\');
    quoted.push_back('"');
    return quoted;
#else
    std::string quoted = "'";
    for (char ch : value) {
        if (ch == '\'') {
            quoted += "'\\''";
        } else {
            quoted.push_back(ch);
        }
    }
    quoted.push_back('\'');
    return quoted;
#endif
}

std::string JoinCommand(const std::vector<std::string>& args)
{
    std::ostringstream inner;
    for (size_t index = 0; index < args.size(); ++index) {
        if (index > 0) {
            inner << ' ';
        }
        inner << ShellQuote(args[index]);
    }
#if defined(_WIN32)
    inner << " 2>NUL";
    // _popen delegates to cmd.exe. When the executable path itself is quoted,
    // cmd.exe needs the entire command line wrapped in an extra quote pair.
    return "\"" + inner.str() + "\"";
#else
    inner << " 2>/dev/null";
    return inner.str();
#endif
}

std::string ReadCommandStdout(const std::string& command)
{
    std::array<char, 4096> buffer {};
    std::string output;

#if defined(_WIN32)
    FILE* pipe = _popen(command.c_str(), "r");
#else
    FILE* pipe = popen(command.c_str(), "r");
#endif
    if (!pipe) {
        return {};
    }

    while (std::fgets(buffer.data(), static_cast<int>(buffer.size()), pipe)) {
        output += buffer.data();
    }

#if defined(_WIN32)
    const int status = _pclose(pipe);
#else
    const int status = pclose(pipe);
#endif
    if (status != 0) {
        return {};
    }
    return Trim(output);
}

bool ReadCommandBytes(const std::string& command, std::vector<char>* output)
{
    std::array<char, 64 * 1024> buffer {};

#if defined(_WIN32)
    FILE* pipe = _popen(command.c_str(), "rb");
#else
    FILE* pipe = popen(command.c_str(), "r");
#endif
    if (!pipe) {
        return false;
    }

    output->clear();
    while (true) {
        const size_t bytesRead = std::fread(buffer.data(), 1, buffer.size(), pipe);
        if (bytesRead > 0) {
            output->insert(output->end(), buffer.data(), buffer.data() + bytesRead);
        }
        if (bytesRead < buffer.size()) {
            if (std::feof(pipe)) {
                break;
            }
            if (std::ferror(pipe)) {
#if defined(_WIN32)
                _pclose(pipe);
#else
                pclose(pipe);
#endif
                output->clear();
                return false;
            }
        }
    }

#if defined(_WIN32)
    const int status = _pclose(pipe);
#else
    const int status = pclose(pipe);
#endif
    if (status != 0) {
        output->clear();
        return false;
    }
    return true;
}

std::shared_ptr<ArAsset> OpenRemoteAsset(const std::string& url)
{
    const std::string executable = GetEnv("ADS_RESOLVER_HTTP_EXECUTABLE", "curl");
    const std::string bearerToken = GetEnv("ADS_RESOLVER_HTTP_BEARER_TOKEN");
    const std::string timeoutSeconds = GetEnv("ADS_RESOLVER_HTTP_TIMEOUT_SECONDS", "30");

    std::vector<std::string> args {
        executable,
        "--location",
        "--fail",
        "--silent",
        "--show-error",
    };
    if (!timeoutSeconds.empty()) {
        args.push_back("--max-time");
        args.push_back(timeoutSeconds);
    }
    if (!bearerToken.empty()) {
        args.push_back("--header");
        args.push_back("Authorization: Bearer " + bearerToken);
    }
    args.push_back(url);

    const std::string command = JoinCommand(args);
    if (DebugEnabled()) {
        std::cerr << "ADS Resolver remote asset download `" << url << "` using `" << executable
                  << "`\n";
    }

    std::vector<char> bytes;
    if (!ReadCommandBytes(command, &bytes)) {
        if (DebugEnabled()) {
            std::cerr << "ADS Resolver failed to download remote asset `" << url << "`\n";
        }
        return {};
    }

    auto storage = std::make_shared<std::vector<char>>(std::move(bytes));
    std::shared_ptr<const char> buffer(storage, storage->empty() ? nullptr : storage->data());
    return ArInMemoryAsset::FromBuffer(std::move(buffer), storage->size());
}

std::string ResolveWithAdsCli(const std::string& assetPath)
{
    const std::string executable = GetEnv("ADS_RESOLVER_EXECUTABLE", "ads");
    const std::string store = GetEnv("ADS_RESOLVER_STORE");
    const std::string workspace = GetEnv("ADS_RESOLVER_WORKSPACE");
    const std::string mode = GetEnv("ADS_RESOLVER_MODE", "local");
    const std::string remoteBaseUrl = GetEnv("ADS_RESOLVER_REMOTE_BASE_URL");

    if (store.empty()) {
        if (DebugEnabled()) {
            std::cerr << "ADS Resolver: ADS_RESOLVER_STORE is not set\n";
        }
        return {};
    }

    std::vector<std::string> args {
        executable,
        "resolve",
        "--store",
        store,
    };
    if (!workspace.empty()) {
        args.push_back("--workspace");
        args.push_back(workspace);
    }
    args.push_back("--mode");
    args.push_back(mode);
    if (!remoteBaseUrl.empty()) {
        args.push_back("--remote-base-url");
        args.push_back(remoteBaseUrl);
    }
    args.push_back(assetPath);

    const std::string command = JoinCommand(args);
    if (DebugEnabled()) {
        std::cerr << "ADS Resolver command: " << command << "\n";
    }

    const std::string resolved = ReadCommandStdout(command);
    if (DebugEnabled()) {
        std::cerr << "ADS Resolver resolved `" << assetPath << "` -> `" << resolved << "`\n";
    }
    return resolved;
}

} // namespace

class AdsResolver final : public ArResolver
{
public:
    AdsResolver() = default;
    ~AdsResolver() override = default;

protected:
    std::string _CreateIdentifier(
        const std::string& assetPath,
        const ArResolvedPath& anchorAssetPath) const override
    {
        if (StartsWithAdsScheme(assetPath)) {
            return NormalizeSlashes(assetPath);
        }
        if (!anchorAssetPath.empty() && !std::filesystem::path(assetPath).is_absolute()) {
            const std::filesystem::path anchor(anchorAssetPath.GetPathString());
            return NormalizeSlashes((anchor.parent_path() / assetPath).lexically_normal().string());
        }
        return NormalizeSlashes(std::filesystem::path(assetPath).lexically_normal().string());
    }

    std::string _CreateIdentifierForNewAsset(
        const std::string& assetPath,
        const ArResolvedPath& anchorAssetPath) const override
    {
        return _CreateIdentifier(assetPath, anchorAssetPath);
    }

    ArResolvedPath _Resolve(const std::string& assetPath) const override
    {
        if (!StartsWithAdsScheme(assetPath)) {
            return {};
        }
        const std::string resolved = NormalizeSlashes(ResolveWithAdsCli(assetPath));
        if (resolved.empty()) {
            return {};
        }
        return ArResolvedPath(resolved);
    }

    ArResolvedPath _ResolveForNewAsset(const std::string& assetPath) const override
    {
        // ADS URI writes are intentionally disabled for the first resolver phase.
        // Artists should create/edit explicit workspace version folders and publish
        // through `ads add`.
        return StartsWithAdsScheme(assetPath) ? ArResolvedPath() : ArResolvedPath(assetPath);
    }

    bool _IsContextDependentPath(const std::string& assetPath) const override
    {
        return StartsWithAdsScheme(assetPath);
    }

    ArTimestamp _GetModificationTimestamp(
        const std::string& assetPath,
        const ArResolvedPath& resolvedPath) const override
    {
        if (resolvedPath.empty()) {
            return {};
        }
        if (DebugEnabled()) {
            std::cerr << "ADS Resolver timestamp `" << assetPath << "` resolved `"
                      << resolvedPath.GetPathString() << "`\n";
        }
        return ArFilesystemAsset::GetModificationTimestamp(resolvedPath);
    }

    std::shared_ptr<ArAsset> _OpenAsset(const ArResolvedPath& resolvedPath) const override
    {
        if (resolvedPath.empty()) {
            return {};
        }
        if (DebugEnabled()) {
            std::cerr << "ADS Resolver open asset `" << resolvedPath.GetPathString() << "`\n";
        }
        if (StartsWithHttpScheme(resolvedPath.GetPathString())) {
            return OpenRemoteAsset(resolvedPath.GetPathString());
        }
        auto asset = ArFilesystemAsset::Open(resolvedPath);
        if (!asset && DebugEnabled()) {
            std::cerr << "ADS Resolver failed to open `" << resolvedPath.GetPathString() << "`\n";
        }
        return asset;
    }

    bool _CanWriteAssetToPath(
        const ArResolvedPath& resolvedPath,
        std::string* whyNot) const override
    {
        if (whyNot) {
            *whyNot = "ADS resolver is read-only; create/edit a workspace version folder and publish with ads add";
        }
        return false;
    }

    std::shared_ptr<ArWritableAsset> _OpenAssetForWrite(
        const ArResolvedPath& resolvedPath,
        WriteMode writeMode) const override
    {
        return {};
    }
};

AR_DEFINE_RESOLVER(AdsResolver, ArResolver);

PXR_NAMESPACE_CLOSE_SCOPE
