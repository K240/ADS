"""Houdini USD ROP output processor for ADS managed publish."""

from ads.houdini_output import AdsPathMapper
from husd.outputprocessor import OutputProcessor


class AdsPublishOutputProcessor(OutputProcessor):
    @staticmethod
    def name():
        return "adspublish"

    @staticmethod
    def displayName():
        return "ADS Managed Publish"

    def __init__(self):
        super().__init__()
        self._mapper = AdsPathMapper.from_environment()

    def processSavePath(self, asset_path, referencing_layer_path, asset_is_layer):
        return self._mapper.to_public_save_path(asset_path)

    def processReferencePath(self, asset_path, referencing_layer_path, asset_is_layer):
        return self._mapper.to_ads_uri(asset_path)

    def processReferenceExpression(self, asset_path, referencing_layer_path, asset_is_layer):
        return self._mapper.to_ads_uri(asset_path)


def usdOutputProcessor():
    return AdsPublishOutputProcessor()
