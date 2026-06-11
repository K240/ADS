"""Houdini USD ROP output processor for ADS WIP staging (schema v8).

Save paths under the configured department root are redirected to a unique
staging run, so a write never overwrites bytes another process holds open.
Register the staged result from the ROP post-render script:

    from ads.houdini_wip import commit_staged
    commit_staged()
"""

from ads.houdini_wip import WipStaging
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

    def processSavePath(self, asset_path, referencing_layer_path, asset_is_layer):
        return self._staging.redirect(asset_path)


def usdOutputProcessor():
    return AdsWipStagingOutputProcessor()
