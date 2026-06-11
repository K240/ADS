"""Refresh the ADS resolver inside a running USD host (Houdini, usdview).

The C++ resolver caches resolve results: explicit ``?v=N`` pins for the
session, ``current``/``latest`` for a short TTL. Open stages additionally
hold on to already-resolved paths until the resolver announces a change.
:func:`refresh` drops the resolver caches and sends
``ArNotice::ResolverChanged`` so listening hosts re-resolve ``ads://``
paths — use it after moving ``current`` on the server to see the new
version without reloading files or restarting.

``ArResolver.RefreshContext`` only reaches the primary resolver in the USD
builds current DCCs ship, so this module additionally calls the plugin's
exported ``AdsResolverRefreshCaches`` entry point directly on the loaded
module.

Houdini shelf tool / pipeline menu::

    import ads.usd_refresh
    ads.usd_refresh.refresh()
"""

from __future__ import annotations

import ctypes

__all__ = ["refresh"]

_PLUGIN_MODULE_NAMES = ("AdsResolver.dll", "AdsResolver.so", "libAdsResolver.so")


def refresh() -> bool:
    """Clear ADS resolver caches and notify the host to re-resolve.

    Returns True when the ADS plugin caches were cleared directly; False
    when the plugin module was not loaded in this process (the generic
    ``RefreshContext`` call is still issued for the primary resolver).
    """
    try:
        from pxr import Ar
    except ImportError as error:  # pragma: no cover - depends on host runtime
        raise RuntimeError(
            "ads.usd_refresh requires a USD-enabled Python; run it inside "
            "Houdini, usdview, or another USD host process"
        ) from error
    Ar.GetResolver().RefreshContext(Ar.ResolverContext())
    return _clear_plugin_caches()


def _clear_plugin_caches() -> bool:
    """Call AdsResolverRefreshCaches on the already-loaded plugin module."""
    for name in _PLUGIN_MODULE_NAMES:
        try:
            module = ctypes.CDLL(name)
        except OSError:
            continue
        entry = getattr(module, "AdsResolverRefreshCaches", None)
        if entry is None:
            continue
        entry.restype = None
        entry()
        return True
    return False
