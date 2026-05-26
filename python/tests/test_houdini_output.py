import os
import unittest

from ads.houdini_output import AdsPathMapper


class AdsPathMapperTests(unittest.TestCase):
    def test_workspace_path_maps_to_version_pinned_ads_uri(self):
        mapper = AdsPathMapper(workspace_root=r"D:\workspace")

        uri = mapper.to_ads_uri(
            r"D:\workspace\char\hero\model\v003\geo\hero_body.usd"
        )

        self.assertEqual(uri, "ads://char/hero/model/geo/hero_body.usd?v=v003")

    def test_nested_category_maps_to_ads_uri(self):
        mapper = AdsPathMapper(workspace_root=r"D:\workspace")

        uri = mapper.to_ads_uri(
            r"D:\workspace\assets\characters\main\hero\texture\v012\maps\body.1001.tx"
        )

        self.assertEqual(
            uri,
            "ads://assets/characters/main/hero/texture/maps/body.1001.tx?v=v012",
        )

    def test_public_path_maps_to_ads_uri(self):
        mapper = AdsPathMapper(
            workspace_root=r"D:\workspace",
            public_root=r"D:\public",
        )

        uri = mapper.to_ads_uri(r"D:\public\prop\crate\model\v001\crate.usd")

        self.assertEqual(uri, "ads://prop/crate/model/crate.usd?v=v001")

    def test_workspace_save_path_can_be_redirected_to_public_root(self):
        mapper = AdsPathMapper(
            workspace_root=r"D:\workspace",
            public_root=r"D:\public",
        )

        path = mapper.to_public_save_path(r"D:\workspace\prop\crate\model\v001\crate.usd")

        self.assertEqual(path.replace(os.sep, "/"), "D:/public/prop/crate/model/v001/crate.usd")

    def test_unmanaged_path_is_left_unchanged(self):
        mapper = AdsPathMapper(workspace_root=r"D:\workspace")

        path = r"E:\external\texture\body.tx"

        self.assertEqual(mapper.to_ads_uri(path), path)

    def test_version_query_can_be_disabled(self):
        mapper = AdsPathMapper(
            workspace_root=r"D:\workspace",
            include_version_query=False,
        )

        uri = mapper.to_ads_uri(r"D:\workspace\prop\crate\model\v001\crate.usd")

        self.assertEqual(uri, "ads://prop/crate/model/crate.usd")


if __name__ == "__main__":
    unittest.main()
