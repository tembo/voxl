// Poll the root window for the loadgen marker; report marker->root visibility latency.
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/time.h>
#include <time.h>
int main(int c,char**v){int mx=c>1?atoi(v[1]):300,my=c>2?atoi(v[2]):150,secs=c>3?atoi(v[3]):10;
Display*d=XOpenDisplay(0);Window r=DefaultRootWindow(d);long last=-1;double sum=0;int n=0;int hist[200]={0};
struct timeval t0;gettimeofday(&t0,0);
for(;;){struct timeval tv;gettimeofday(&tv,0);if(tv.tv_sec-t0.tv_sec>=secs)break;
XImage*im=XGetImage(d,r,mx,my,480,48,AllPlanes,ZPixmap);uint64_t val=0;
for(int b=0;b<40;b++){int x=(b%20)*24+12,y=(b/20)*24+12;unsigned long p=XGetPixel(im,x,y);if(((p>>16)&0xff)>128)val|=1ULL<<b;}
XDestroyImage(im);long fr=val>>24;uint32_t ms=val&0xffffff;
if(fr!=last){last=fr;uint32_t now=(uint32_t)((tv.tv_sec*1000LL+tv.tv_usec/1000)&0xffffff);int l=(int)now-(int)ms;if(l>=0&&l<200){sum+=l;n++;hist[l]++;}}
struct timespec ts={0,500000};nanosleep(&ts,0);}
int acc=0,p50=-1,p90=-1;for(int i=0;i<200;i++){acc+=hist[i];if(p50<0&&acc>=n/2)p50=i;if(p90<0&&acc>=n*9/10)p90=i;}
printf("root-visible latency: frames=%d avg=%.1fms p50=%d p90=%d\n",n,sum/n,p50,p90);}
