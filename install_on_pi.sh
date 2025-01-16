#!/bin/bash
sudo cp ac.service /etc/systemd/system
sudo systemctl enable systemd-networkd.service systemd-networkd-wait-online.service ac.service
sudo mv local.network /etc/systemd/network
sudo rm /etc/systemd/system/getty.target.wants/getty*
mkdir logs
sudo shutdown -r now
