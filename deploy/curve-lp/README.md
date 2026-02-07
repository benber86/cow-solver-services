# Curve LP Solver - Deployment Guide

## Prerequisites

1. **AWS Account** with permissions to create EC2, Security Groups
2. **Ethereum RPC URL** (Alchemy, Infura, or your own node)
3. **Solver private key** (dedicated wallet for signing settlements)
4. **Domain** pointing to your server (solver.momotaro.xyz → EC2 IP)
5. **AWS CLI** configured locally (`aws configure`)

---

## Step 1: Create EC2 Instance

### Via AWS Console:

1. Go to **EC2 → Launch Instance**

2. **Settings:**
   | Setting | Value |
   |---------|-------|
   | Name | `curve-lp-solver` |
   | AMI | Amazon Linux 2023 or Ubuntu 22.04 |
   | Instance type | `t3.small` (start small, upgrade if needed) |
   | Key pair | Create new or use existing |
   | Storage | 20 GB gp3 |

3. **Network settings:**
   - Create new security group
   - Allow SSH (port 22) from your IP
   - Allow port 8080 from `0.0.0.0/0` (or restrict to CoW IPs later)

4. **Launch** and note the instance ID

### Via AWS CLI:
```bash
# Create security group
aws ec2 create-security-group \
  --group-name curve-lp-solver-sg \
  --description "Curve LP Solver"

# Allow SSH
aws ec2 authorize-security-group-ingress \
  --group-name curve-lp-solver-sg \
  --protocol tcp --port 22 --cidr YOUR_IP/32

# Allow solver port
aws ec2 authorize-security-group-ingress \
  --group-name curve-lp-solver-sg \
  --protocol tcp --port 8080 --cidr 0.0.0.0/0

# Launch instance
aws ec2 run-instances \
  --image-id ami-0c55b159cbfafe1f0 \
  --instance-type t3.small \
  --key-name YOUR_KEY_NAME \
  --security-groups curve-lp-solver-sg \
  --block-device-mappings '[{"DeviceName":"/dev/xvda","Ebs":{"VolumeSize":20,"VolumeType":"gp3"}}]' \
  --tag-specifications 'ResourceType=instance,Tags=[{Key=Name,Value=curve-lp-solver}]'
```

---

## Step 2: Allocate Elastic IP

```bash
# Allocate
aws ec2 allocate-address --domain vpc

# Note the AllocationId, then associate with your instance
aws ec2 associate-address \
  --instance-id i-YOUR_INSTANCE_ID \
  --allocation-id eipalloc-YOUR_ALLOCATION_ID
```

Note down the Elastic IP - this is your solver's public address.

---

## Step 3: SSH into Instance

```bash
ssh -i ~/.ssh/your-key.pem ec2-user@YOUR_ELASTIC_IP
# or for Ubuntu:
ssh -i ~/.ssh/your-key.pem ubuntu@YOUR_ELASTIC_IP
```

---

## Step 4: Install Docker

### Amazon Linux 2023:
```bash
sudo yum update -y
sudo yum install -y docker git
sudo systemctl start docker
sudo systemctl enable docker
sudo usermod -aG docker $USER

# Install docker-compose
sudo curl -L "https://github.com/docker/compose/releases/latest/download/docker-compose-$(uname -s)-$(uname -m)" -o /usr/local/bin/docker-compose
sudo chmod +x /usr/local/bin/docker-compose

# Log out and back in for group to take effect
exit
```

### Ubuntu 22.04:
```bash
sudo apt update
sudo apt install -y docker.io docker-compose git
sudo systemctl start docker
sudo systemctl enable docker
sudo usermod -aG docker $USER

# Log out and back in
exit
```

---

## Step 5: Clone and Setup

```bash
# SSH back in
ssh -i ~/.ssh/your-key.pem ec2-user@YOUR_ELASTIC_IP

# Clone the repo
git clone https://github.com/YOUR_ORG/cow-solver-services.git
cd cow-solver-services/deploy/curve-lp

# Create .env from template
cp .env.example .env

# Edit with your secrets
nano .env
```

### Fill in `.env`:
```bash
NODE_URL=https://eth-mainnet.g.alchemy.com/v2/YOUR_ACTUAL_KEY
SOLVER_ACCOUNT=0xYOUR_ACTUAL_PRIVATE_KEY
```

**Important:**
- The `SOLVER_ACCOUNT` wallet needs some ETH for gas (~0.1 ETH to start)
- This wallet will be used to sign settlement transactions

---

## Step 6: Deploy

```bash
# Make sure you're in deploy/curve-lp
cd ~/cow-solver-services/deploy/curve-lp

# Run deployment
./deploy.sh
```

This will:
1. Validate your environment variables
2. Process config files (substitute secrets)
3. Build and start the containers

---

## Step 7: Verify

```bash
# Check containers are running
docker-compose -f docker-compose.prod.yml ps

# Check logs
docker-compose -f docker-compose.prod.yml logs -f

# Test health endpoint
curl http://localhost:8080/healthz

# Test from outside (use your Elastic IP)
curl http://YOUR_ELASTIC_IP:8080/healthz
```

---

## Step 8: Register with CoW Protocol

Your solver is running at `http://YOUR_ELASTIC_IP:8080`

To participate in CoW Protocol auctions, you need to:

1. **Contact CoW Protocol team** via:
   - Discord: https://discord.com/invite/cowprotocol
   - Or their solver onboarding process

2. **Provide:**
   - Your solver endpoint: `http://YOUR_ELASTIC_IP:8080`
   - Solver name: `curve-lp` (or your choice)
   - What orders you handle: Curve LP token sells

3. **Bond requirement:**
   - Check current bonding requirements with CoW team
   - May need to stake COW tokens

---

## Monitoring & Maintenance

### View logs:
```bash
docker-compose -f docker-compose.prod.yml logs -f solver  # Solver only
docker-compose -f docker-compose.prod.yml logs -f driver  # Driver only
docker-compose -f docker-compose.prod.yml logs -f         # Both
```

### Restart services:
```bash
docker-compose -f docker-compose.prod.yml restart
```

### Update to latest code:
```bash
cd ~/cow-solver-services
git pull
cd deploy/curve-lp
docker-compose -f docker-compose.prod.yml up -d --build
```

### Stop services:
```bash
docker-compose -f docker-compose.prod.yml down
```

---

## Cost Estimate

| Resource | Monthly Cost |
|----------|-------------|
| EC2 t3.small | ~$15 |
| Elastic IP | Free (when attached) |
| Storage 20GB | ~$2 |
| Data transfer | ~$1-5 |
| **Total** | **~$20-25/mo** |

---

## Troubleshooting

### Container won't start:
```bash
docker-compose -f docker-compose.prod.yml logs
```

### Can't connect to port 8080:
- Check security group allows 8080
- Check `docker ps` shows containers running
- Check firewall: `sudo iptables -L`

### Solver not finding routes:
- Check Curve API is accessible: `curl https://api.curve.fi`
- Check RPC is working: logs will show RPC errors

### Out of memory:
- Upgrade to t3.medium
- Or add swap: `sudo fallocate -l 2G /swapfile && sudo mkswap /swapfile && sudo swapon /swapfile`

---

## Security Hardening (Optional)

1. **Restrict SSH access** to your IP only
2. **Restrict port 8080** to CoW Protocol IPs (ask them for IP ranges)
3. **Enable CloudWatch** for log monitoring
4. **Set up alerts** for container restarts
5. **Use AWS Secrets Manager** instead of .env file (see AWS_SECRETS.md)
