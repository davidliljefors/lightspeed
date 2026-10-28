from LSP.plugin import AbstractPlugin, register_plugin, unregister_plugin


class Lightspeed(AbstractPlugin):
    @classmethod
    def name(cls) -> str:
        return "lightspeed"


def plugin_loaded() -> None:
    register_plugin(Lightspeed)


def plugin_unloaded() -> None:
    unregister_plugin(Lightspeed)
