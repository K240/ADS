"""Houdini USD ROP output processor for ADS WIP staging (schema v8).

Save paths under the configured department root are redirected to a unique
staging run, so a write never overwrites bytes another process holds open.
Register the staged result from the ROP post-render script:

    from ads.houdini_wip import commit_staged
    commit_staged()
"""

import os

from ads.houdini_wip import WipStaging, process_run_id
from husd.outputprocessor import OutputProcessor


class AdsWipStagingOutputProcessor(OutputProcessor):
    @staticmethod
    def name():
        return "adswipstaging"

    @staticmethod
    def displayName():
        return "ADS WIP Staging"

    def __init__(self):
        super().__init__()
        self._staging = WipStaging.from_environment()
        # husd builds a fresh processor per save, but one render saves the
        # root layer and sublayers separately: every save joins the
        # interpreter's shared run so the post-render commit_staged()
        # registers them as a single micro-version. The run id deliberately
        # stays out of os.environ — it leaks into child processes there and
        # cross-links unrelated renders. An explicit ADS_WIP_RUN_ID pin wins.
        if not os.environ.get("ADS_WIP_RUN_ID"):
            self._staging.run_id = process_run_id()

    def processSavePath(self, asset_path, referencing_layer_path, asset_is_layer):
        return self._staging.redirect(asset_path)


def usdOutputProcessor():
    return AdsWipStagingOutputProcessor()
