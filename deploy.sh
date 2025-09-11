#! /bin/bash
# This Script is transferring the page to the webspace, where the page is hosted.

SSH_HOST="ssh.strato.de"
SSH_USER="510463487.swh.strato-hosting.eu"
TARGET_LOCATION="start/"

set -x
# I dont want to transfer the repo data to the webspace
rsync --exclude=.git/ -avz . "$SSH_USER@$SSH_HOST:$TARGET_LOCATION"
