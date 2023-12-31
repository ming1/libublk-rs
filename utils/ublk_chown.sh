#!/bin/bash
# SPDX-License-Identifier: MIT or Apache-2.0

MY_DIR=$(cd "$(dirname "$0")";pwd)

if ID=`${MY_DIR}/ublk_user_id $1 2>/dev/null`; then
	if [ "$2" == "add" ]; then
		/usr/bin/chown $ID /dev/$1 > /dev/null 2>&1
	fi
fi
