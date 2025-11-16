# Fluxion Docker Deployment Guide

This guide explains how to deploy Fluxion using Docker containers for both development and
production environments.

## Quick Start (Development)

1. **Prerequisites**

   ```bash
   # Install Docker and Docker Compose
   curl -fsSL https://get.docker.com | sh
   sudo usermod -aG docker $USER
   # Log out and back in for group changes to take effect
   ```

2. **Setup Environment**

   ```bash
   # Clone the repository
   git clone <repository-url>
   cd solare/main

   # Copy and customize environment variables
   cp .env.example .env
   # Edit .env with your specific configuration
   ```

3. **Start Development Environment**

   ```bash
   # Make the development script executable
   chmod +x docker-dev.sh

   # Start all services
   ./docker-dev.sh start

   # Or start specific services
   ./docker-dev.sh start --services "postgres redis influxdb"
   ```

4. **Access Services**

   - Fluxion Server: http://localhost:8080
   - Fluxion Client: http://localhost:8000
   - Node-RED: http://localhost:1880
   - Grafana: http://localhost:3000
   - Prometheus: http://localhost:9090

## Production Deployment

### System Requirements

**Minimum Requirements:**

- CPU: 2 cores
- RAM: 4GB
- Storage: 50GB SSD
- Network: 1Gbps

**Recommended Requirements:**

- CPU: 4+ cores
- RAM: 8GB+
- Storage: 100GB+ SSD
- Network: 1Gbps+

### Production Setup

1. **Prepare Production Environment**

   ```bash
   # Create data directories
   sudo mkdir -p /opt/fluxion/{data,config,logs,certs}

   # Set ownership
   sudo chown -R $USER:$USER /opt/fluxion

   # Create production environment file
   cp .env.example .env.prod
   # Configure production settings in .env.prod
   ```

2. **Generate SSL Certificates**

   ```bash
   # For production, use Let's Encrypt or your CA certificates
   # Example with Let's Encrypt (adjust domain name):
   certbot certonly --standalone -d yourdomain.com

   # Copy certificates
   cp /etc/letsencrypt/live/yourdomain.com/fullchain.pem certs/server.crt
   cp /etc/letsencrypt/live/yourdomain.com/privkey.pem certs/server.key

   # Generate JWT keys
   openssl genpkey -algorithm RSA -out certs/jwt_private.pem -pkcs8
   openssl pkey -in certs/jwt_private.pem -pubout -out certs/jwt_public.pem
   ```

3. **Deploy Production Stack**

   ```bash
   # Deploy with production compose file
   docker compose -f docker-compose.prod.yml --env-file .env.prod up -d

   # Monitor deployment
   docker compose -f docker-compose.prod.yml logs -f
   ```

## Service Architecture

### Core Services

- **fluxion-server**: Main application server (Rust/Axum)
- **fluxion-client**: Solar plant client with Node-RED (Rust + Node.js)
- **postgres**: PostgreSQL database for relational data
- **influxdb**: InfluxDB for time-series solar metrics
- **redis**: Redis for caching, sessions, and message queuing
- **mosquitto**: MQTT broker for IoT device communication

### Monitoring Stack

- **grafana**: Visualization and dashboards
- **prometheus**: Metrics collection and alerting
- **alertmanager**: Alert routing and management
- **nginx**: Reverse proxy and load balancer

### Optional Services

- **filebeat**: Log shipping (with `--profile logging`)
- **backup**: Automated backup service (with `--profile backup`)
- **dev-tools**: Development utilities (with `--profile dev`)

## Configuration

### Environment Variables

Key environment variables to configure:

```bash
# Security (REQUIRED for production)
POSTGRES_PASSWORD=secure_password_here
REDIS_PASSWORD=secure_redis_password
INFLUXDB_PASSWORD=secure_influxdb_password
GRAFANA_PASSWORD=secure_grafana_password

# Client Configuration
CLIENT_ID=your_unique_client_id
INVERTER_HOST=192.168.1.100
INVERTER_SERIAL=your_inverter_serial

# Feature Flags
ENABLE_TLS=true          # Enable HTTPS in production
ENABLE_TOR=false         # Enable Tor for anonymity
ENABLE_PRIVILEGED=false  # Enable hardware access for client
```

### Data Persistence

Data is persisted in Docker volumes:

- `postgres-data`: PostgreSQL database
- `influxdb-data`: InfluxDB time-series data
- `redis-data`: Redis cache and sessions
- `grafana-data`: Grafana dashboards and settings
- `fluxion-server-data`: Server application data
- `fluxion-client-data`: Client application data

### Networking

Services communicate over the `fluxion-net` bridge network:

- External services expose ports on host
- Internal services use container names for DNS resolution
- Monitoring services use separate `monitoring-net` for security

## Management Commands

### Development Script Usage

```bash
# Start services
./docker-dev.sh start

# Stop services  
./docker-dev.sh stop

# View logs
./docker-dev.sh logs --services "fluxion-server"

# Get shell access
./docker-dev.sh shell fluxion-server

# Run tests
./docker-dev.sh test

# Clean up (including data volumes)
./docker-dev.sh clean --clean-volumes

# Show service status
./docker-dev.sh status

# Create backup
./docker-dev.sh backup
```

### Manual Docker Compose Commands

```bash
# Start all services
docker compose up -d

# Start specific services
docker compose up -d postgres redis influxdb

# View logs
docker compose logs -f fluxion-server

# Scale services
docker compose up -d --scale fluxion-client=3

# Update services
docker compose pull
docker compose up -d

# Stop services
docker compose down

# Remove everything including volumes
docker compose down -v
```

## Monitoring and Troubleshooting

### Health Checks

All services include health checks. Check status:

```bash
# View health status
docker compose ps

# Check specific service health
docker inspect fluxion-server --format='{{.State.Health.Status}}'

# View health check logs
docker inspect fluxion-server --format='{{range .State.Health.Log}}{{.Output}}{{end}}'
```

### Logging

View logs from different services:

```bash
# All services
docker compose logs -f

# Specific service
docker compose logs -f fluxion-server

# With timestamps
docker compose logs -f --timestamps

# Last N lines
docker compose logs --tail=100 fluxion-server
```

### Resource Monitoring

Monitor resource usage:

```bash
# Container resource usage
docker stats

# System resource usage
docker system df

# Clean up unused resources
docker system prune
```

## Backup and Recovery

### Automated Backups

Enable backup service:

```bash
# Start with backup profile
docker compose --profile backup up -d

# Manual backup
docker compose exec postgres pg_dump -U fluxion fluxion | gzip > backup.sql.gz
```

### Restore from Backup

```bash
# Stop services
docker compose down

# Restore PostgreSQL
gunzip -c backup.sql.gz | docker compose exec -T postgres psql -U fluxion -d fluxion

# Start services
docker compose up -d
```

## Security Considerations

### Production Security Checklist

- [ ] Use strong, unique passwords for all services
- [ ] Enable TLS/SSL for all external connections
- [ ] Configure firewall to restrict port access
- [ ] Use non-root users in containers
- [ ] Regularly update container images
- [ ] Monitor logs for suspicious activity
- [ ] Enable audit logging
- [ ] Backup encryption keys securely
- [ ] Use secrets management for sensitive data
- [ ] Network segmentation for monitoring services

### Hardening Steps

1. **Container Security**

   ```bash
   # Run containers as non-root
   # Use read-only filesystems where possible
   # Drop unnecessary capabilities
   # Use security profiles (AppArmor/SELinux)
   ```

2. **Network Security**

   ```bash
   # Use internal networks for service communication
   # Expose only necessary ports
   # Enable TLS for all external connections
   # Use MQTT over TLS (port 8883)
   ```

3. **Data Security**

   ```bash
   # Encrypt data at rest
   # Use encrypted database connections
   # Secure backup storage
   # Rotate credentials regularly
   ```

## Troubleshooting

### Common Issues

1. **Services won't start**

   ```bash
   # Check logs
   docker compose logs service-name

   # Check available resources
   docker system df
   docker stats

   # Verify configuration
   docker compose config
   ```

2. **Database connection errors**

   ```bash
   # Check database status
   docker compose exec postgres pg_isready -U fluxion

   # Verify credentials in .env file
   # Check network connectivity
   docker compose exec fluxion-server ping postgres
   ```

3. **High resource usage**

   ```bash
   # Monitor resource usage
   docker stats

   # Adjust resource limits in compose files
   # Scale down services if needed
   ```

4. **Port conflicts**

   ```bash
   # Check which ports are in use
   netstat -tlnp

   # Modify port mappings in compose file
   # Use different external ports
   ```

### Debug Mode

Enable debug mode for troubleshooting:

```bash
# Set debug environment variables
export LOG_LEVEL=debug
export RUST_BACKTRACE=full

# Restart services with debug logging
docker compose up -d --force-recreate
```

## Performance Tuning

### Database Optimization

1. **PostgreSQL**

   - Adjust `shared_buffers` for available memory
   - Tune `work_mem` for query performance
   - Configure appropriate connection pooling

2. **InfluxDB**

   - Set appropriate retention policies
   - Configure shard group duration
   - Optimize cache settings

3. **Redis**

   - Set appropriate memory limits
   - Configure eviction policies
   - Enable persistence as needed

### Container Resource Limits

Adjust resource limits based on your system:

```yaml
# Example resource limits
deploy:
  resources:
    limits:
      memory: 1G
      cpus: '1.0'
    reservations:
      memory: 512M
      cpus: '0.5'
```

## Support

For additional support:

1. Check the logs: `docker compose logs -f`
2. Verify configuration: `docker compose config`
3. Review this documentation
4. Check the main project README
5. File an issue in the project repository
