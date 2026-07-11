# complete code

import pytest
from src.sync.config import SyncConfig, Error

def test_valid_project_config():
    project_config_path = 'test_project_config.yaml'
    sync_config = SyncConfig.load(project_config_path)
    assert sync_config.remote == 'file:///real/cache.git'
    assert sync_config.lfs == False

def test_invalid_project_config():
    project_config_path = 'test_invalid_project_config.yaml'
    with pytest.raises(Error):
        SyncConfig.load(project_config_path)

def test_missing_project_config():
    project_config_path = 'non_existent_project_config.yaml'
    with pytest.raises(Error):
        SyncConfig.load(project_config_path)