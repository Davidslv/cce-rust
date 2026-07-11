# complete code

import serde_yaml
from this_error import Error

class SyncConfig:
    def __init__(self, remote: str, lfs: bool):
        self.remote = remote
        self.lfs = lfs

    @classmethod
    def load(cls, project_config_path: str) -> Result['SyncConfig', Error]:
        try:
            with open(project_config_path, 'r') as f:
                project_config = serde_yaml.safe_load(f)
        except FileNotFoundError:
            return Error(f"Project config file not found at {project_config_path}")
        except serde_yaml.YAMLError as e:
            return Error(f"Failed to parse project config file at {project_config_path}: {e}")

        remote = project_config.get('remote')
        lfs = project_config.get('lfs', False)

        if remote is None or not isinstance(remote, str):
            return Error(f"Missing or invalid 'remote' field in project config file at {project_config_path}")
        if not isinstance(lfs, bool):
            return Error(f"Invalid 'lfs' field in project config file at {project_config_path}")

        return SyncConfig(remote, lfs)

    @classmethod
    def parse_config(cls, project_config_path: str) -> Result['SyncConfig', Error]:
        try:
            with open(project_config_path, 'r') as f:
                project_config = serde_yaml.safe_load(f)
        except FileNotFoundError:
            return Error(f"Project config file not found at {project_config_path}")
        except serde_yaml.YAMLError as e:
            return Error(f"Failed to parse project config file at {project_config_path}: {e}")

        return cls._parse_config(project_config)

    @classmethod
    def _parse_config(cls, project_config: dict) -> Result['SyncConfig', Error]:
        remote = project_config.get('remote')
        lfs = project_config.get('lfs', False)

        if remote is None or not isinstance(remote, str):
            return Error(f"Missing or invalid 'remote' field in project config file")
        if not isinstance(lfs, bool):
            return Error(f"Invalid 'lfs' field in project config file")

        return SyncConfig(remote, lfs)

def get_sync_config(project_config_path: str) -> Result['SyncConfig', Error]:
    return SyncConfig.load(project_config_path)